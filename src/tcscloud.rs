//! Tcscloud Seednet dynamic-IP VPS.
//!
//! The vendor API (`/getip`, `/changeip`) is only reachable from inside the VPS, so every call goes
//! through SSH. Changing the IP drops the SSH session (the NAT public IP changes), so the change is
//! started detached on the node and then confirmed by polling `/getip` over a fresh SSH session.
//!
//! The old IP is handed to another customer within minutes. While DNS still points at the old IP an
//! SSH connection would land on a stranger's box, so the host key is pinned in a dedicated
//! known_hosts file with strict checking; a mismatch only means "not ready yet".

use std::{
    future::Future,
    net::{IpAddr, Ipv4Addr},
    process::Stdio,
    time::Duration,
};

use serde::Deserialize;
use tokio::{process::Command, time::timeout};

use crate::{config::TcscloudSeednetConfig, fachost::is_public_ipv4};

const PUBLIC_IP_MARKER: &str = "__BOIL_TCS_PUBLIC__";
/// Vendor view + egress view in one SSH session. Always exits 0; parsing decides.
const GET_IP_COMMAND: &str = r#"v=$(curl -sS --connect-timeout 5 --max-time 8 http://ipapi.tcscloud.net:8002/getip 2>/dev/null); printf '%s\n__BOIL_TCS_PUBLIC__\n' "$v"; for url in https://api.ipify.org https://ipv4.icanhazip.com; do p=$(curl -4fsS --connect-timeout 4 --max-time 6 "$url" 2>/dev/null) && printf '%s' "$p" && break; done; exit 0"#;
const CHANGE_STARTED_MARKER: &str = "__BOIL_TCS_CHANGE_STARTED__";
/// Starts `/changeip` detached from the SSH session (it outlives the session the IP change kills).
/// `flock -n` keeps a second start from running while one is still in flight.
const CHANGE_START_COMMAND: &str = r#"rm -f /tmp/boil-tcs-change.json; setsid nohup flock -n /tmp/boil-tcs-change.lock sh -c 'curl -sS --max-time 90 http://ipapi.tcscloud.net:8002/changeip > /tmp/boil-tcs-change.json 2>&1' >/dev/null 2>&1 </dev/null & echo __BOIL_TCS_CHANGE_STARTED__"#;
const SSH_TIMEOUT: Duration = Duration::from_secs(35);
const MAX_ROUNDS: usize = 2;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TcscloudReconnectPolicy {
    /// Wait after starting the change before the first poll (vendor needs ~30–40 s).
    pub initial_wait: Duration,
    pub poll_interval: Duration,
    /// Polling window per round, counted after `initial_wait`.
    pub confirmation_window: Duration,
    /// Test cap; production is deadline-driven.
    pub max_poll_attempts_per_round: usize,
    /// Hard-capped at 2. A second round only starts when our own box answered with the old IP at
    /// the end of the window (the change really did not happen), never when it was unreachable.
    pub max_rounds: usize,
}

impl Default for TcscloudReconnectPolicy {
    fn default() -> Self {
        Self {
            initial_wait: Duration::from_secs(30),
            poll_interval: Duration::from_secs(10),
            confirmation_window: Duration::from_secs(300),
            max_poll_attempts_per_round: usize::MAX,
            max_rounds: MAX_ROUNDS,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TcscloudReconnectOutcome {
    pub old_ip: IpAddr,
    pub new_ip: Option<IpAddr>,
    pub changed: bool,
    /// At least one change was started (or may have been started).
    pub change_requested: bool,
    /// Our box was reachable at the end of the last window and still had the old IP.
    pub ip_unchanged: bool,
    pub dial_attempts: usize,
    pub poll_attempts: usize,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TcscloudError {
    SshUnavailable,
    HostKeyMismatch,
    Timeout,
    Changing,
    InvalidPublicIpv4,
    InvalidResponse,
}

impl std::fmt::Display for TcscloudError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::SshUnavailable => "无法通过 SSH 连接 Tcscloud Seednet VPS",
            Self::HostKeyMismatch => {
                "Tcscloud Seednet SSH 主机密钥不匹配（域名可能还指向旧 IP），已拒绝连接"
            }
            Self::Timeout => "Tcscloud Seednet VPS 查询超时",
            Self::Changing => "Tcscloud Seednet 正在更换 IP，请稍后再查",
            Self::InvalidPublicIpv4 => "Tcscloud Seednet 返回的公网 IPv4 无效",
            Self::InvalidResponse => "Tcscloud Seednet 返回了无效响应",
        };
        f.write_str(message)
    }
}

impl std::error::Error for TcscloudError {}

#[derive(Clone, Debug, Default)]
pub struct TcscloudSeednetClient;

impl TcscloudSeednetClient {
    pub async fn get_ip(&self, config: &TcscloudSeednetConfig) -> Result<IpAddr, TcscloudError> {
        SshTransport.get_ip(config).await
    }

    pub async fn reconnect(
        &self,
        config: &TcscloudSeednetConfig,
        server_id: &str,
        server_name: &str,
        old_ip: IpAddr,
        policy: &TcscloudReconnectPolicy,
        change_lock: &tokio::sync::Mutex<()>,
    ) -> TcscloudReconnectOutcome {
        reconnect_with_transport(
            &SshTransport,
            config,
            server_id,
            server_name,
            old_ip,
            policy,
            change_lock,
        )
        .await
    }
}

trait TcscloudTransport: Send + Sync {
    fn get_ip(
        &self,
        config: &TcscloudSeednetConfig,
    ) -> impl Future<Output = Result<IpAddr, TcscloudError>> + Send;

    /// `Ok(true)` = start marker seen; `Ok(false)` = command ran but the marker is missing.
    fn start_change(
        &self,
        config: &TcscloudSeednetConfig,
    ) -> impl Future<Output = Result<bool, TcscloudError>> + Send;
}

struct SshTransport;

impl TcscloudTransport for SshTransport {
    async fn get_ip(&self, config: &TcscloudSeednetConfig) -> Result<IpAddr, TcscloudError> {
        let output = run_ssh(config, GET_IP_COMMAND).await?;
        let snapshot = parse_get_ip_output(&output)?;
        if let (Some(vendor), Some(public)) = (snapshot.vendor_ip, snapshot.public_ip) {
            if vendor != public {
                log::warn!(
                    "Tcscloud getip mismatch: vendor_ip={vendor} public_ip={public}; using public egress IP"
                );
            }
        }
        resolve_snapshot(&snapshot)
    }

    async fn start_change(&self, config: &TcscloudSeednetConfig) -> Result<bool, TcscloudError> {
        let output = run_ssh(config, CHANGE_START_COMMAND).await?;
        Ok(output.contains(CHANGE_STARTED_MARKER))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum VendorIp {
    /// API unreachable, unparsable or not `success`.
    Unavailable,
    /// `ip` is empty: a change is in progress.
    Changing,
    Ip(Ipv4Addr),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct IpSnapshot {
    vendor: VendorIp,
    vendor_ip: Option<Ipv4Addr>,
    public_ip: Option<Ipv4Addr>,
}

#[derive(Deserialize)]
struct VendorGetIpResponse {
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    ip: Option<String>,
}

fn parse_get_ip_output(output: &str) -> Result<IpSnapshot, TcscloudError> {
    let (vendor_part, public_part) = output
        .split_once(PUBLIC_IP_MARKER)
        .ok_or(TcscloudError::InvalidResponse)?;
    let vendor = match serde_json::from_str::<VendorGetIpResponse>(vendor_part.trim()) {
        Ok(response)
            if response
                .status
                .as_deref()
                .is_some_and(|status| status.trim().eq_ignore_ascii_case("success")) =>
        {
            match response.ip.as_deref().map(str::trim) {
                Some("") => VendorIp::Changing,
                Some(value) => value
                    .parse::<Ipv4Addr>()
                    .ok()
                    .filter(|ip| is_public_ipv4(*ip))
                    .map(VendorIp::Ip)
                    .unwrap_or(VendorIp::Unavailable),
                None => VendorIp::Unavailable,
            }
        }
        _ => VendorIp::Unavailable,
    };
    let public_ip = public_part
        .trim()
        .parse::<Ipv4Addr>()
        .ok()
        .filter(|ip| is_public_ipv4(*ip));
    Ok(IpSnapshot {
        vendor,
        vendor_ip: match vendor {
            VendorIp::Ip(ip) => Some(ip),
            _ => None,
        },
        public_ip,
    })
}

/// Empty vendor IP = change in progress. Otherwise the egress IP is the ground truth (it is what
/// users and DDNS see); the vendor IP is the fallback when the egress check fails.
fn resolve_snapshot(snapshot: &IpSnapshot) -> Result<IpAddr, TcscloudError> {
    match (snapshot.vendor, snapshot.public_ip) {
        (VendorIp::Changing, _) => Err(TcscloudError::Changing),
        (_, Some(public)) => Ok(IpAddr::V4(public)),
        (VendorIp::Ip(vendor), None) => Ok(IpAddr::V4(vendor)),
        (VendorIp::Unavailable, None) => Err(TcscloudError::InvalidPublicIpv4),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LastPoll {
    None,
    SameIp,
    Changing,
    Failed,
}

async fn reconnect_with_transport<T: TcscloudTransport>(
    transport: &T,
    config: &TcscloudSeednetConfig,
    server_id: &str,
    server_name: &str,
    old_ip: IpAddr,
    policy: &TcscloudReconnectPolicy,
    change_lock: &tokio::sync::Mutex<()>,
) -> TcscloudReconnectOutcome {
    let mut outcome = TcscloudReconnectOutcome {
        old_ip,
        new_ip: None,
        changed: false,
        change_requested: false,
        ip_unchanged: false,
        dial_attempts: 0,
        poll_attempts: 0,
        message: "换 IP 后无法确认新的公网 IPv4".to_string(),
    };

    let max_rounds = policy.max_rounds.clamp(1, MAX_ROUNDS);
    for round in 1..=max_rounds {
        outcome.dial_attempts += 1;
        log::info!(
            "Tcscloud change started: server_id={server_id} server_name={server_name:?} round={round}/{max_rounds} old_ip={old_ip}"
        );
        let start = {
            let _change_guard = change_lock.lock().await;
            transport.start_change(config).await
        };
        match start {
            Ok(true) => {}
            Ok(false) => log::warn!(
                "Tcscloud change start marker missing, confirming anyway: server_id={server_id} round={round}"
            ),
            Err(error @ (TcscloudError::SshUnavailable | TcscloudError::HostKeyMismatch)) => {
                // SSH never reached our box, so the change command did not run.
                log::warn!(
                    "Tcscloud change not started: server_id={server_id} server_name={server_name:?} round={round} error={error}"
                );
                outcome.message = if outcome.ip_unchanged {
                    format!("第 1 次换 IP 后公网 IP 未变化，第 2 次未能发起：{error}")
                } else {
                    format!("未能发起换 IP：{error}")
                };
                return outcome;
            }
            Err(error) => log::warn!(
                "Tcscloud change start result unknown, confirming anyway: server_id={server_id} round={round} error={error}"
            ),
        }
        outcome.change_requested = true;
        outcome.ip_unchanged = false;

        tokio::time::sleep(policy.initial_wait).await;
        let window_started = tokio::time::Instant::now();
        let deadline = window_started + policy.confirmation_window;
        let mut round_polls = 0usize;
        let mut last_poll = LastPoll::None;
        loop {
            if round_polls >= policy.max_poll_attempts_per_round {
                break;
            }
            if round_polls > 0 {
                let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                if remaining.is_zero() {
                    break;
                }
                tokio::time::sleep(policy.poll_interval.min(remaining)).await;
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            round_polls += 1;
            outcome.poll_attempts += 1;
            let elapsed = window_started.elapsed().as_secs() + policy.initial_wait.as_secs();
            match timeout(remaining, transport.get_ip(config)).await {
                Ok(Ok(current_ip)) if current_ip != old_ip => {
                    outcome.new_ip = Some(current_ip);
                    outcome.changed = true;
                    outcome.message = "换 IP 已完成".to_string();
                    log::info!(
                        "Tcscloud change confirmed: server_id={server_id} server_name={server_name:?} round={round} old_ip={old_ip} new_ip={current_ip} elapsed_seconds={elapsed}"
                    );
                    return outcome;
                }
                Ok(Ok(_)) => {
                    last_poll = LastPoll::SameIp;
                    log::info!(
                        "Tcscloud confirmation poll: server_id={server_id} round={round} poll={round_polls} same_ip=true elapsed_seconds={elapsed}"
                    );
                }
                Ok(Err(TcscloudError::Changing)) => {
                    last_poll = LastPoll::Changing;
                    log::info!(
                        "Tcscloud confirmation poll: server_id={server_id} round={round} poll={round_polls} changing=true elapsed_seconds={elapsed}"
                    );
                }
                Ok(Err(error)) => {
                    last_poll = LastPoll::Failed;
                    log::info!(
                        "Tcscloud confirmation poll not ready: server_id={server_id} round={round} poll={round_polls} elapsed_seconds={elapsed} error={error}"
                    );
                }
                Err(_) => {
                    last_poll = LastPoll::Failed;
                    break;
                }
            }
        }

        if last_poll != LastPoll::SameIp {
            let wait_secs = (policy.initial_wait + policy.confirmation_window).as_secs();
            outcome.message = format!(
                "已发起换 IP，但 {} 分钟内未能确认新 IP（VPS 暂不可达或域名未更新），请稍后查看状态",
                wait_secs.div_ceil(60)
            );
            log::warn!(
                "Tcscloud change unconfirmed: server_id={server_id} server_name={server_name:?} round={round} last_poll={last_poll:?} polls={} dial_attempts={}",
                outcome.poll_attempts,
                outcome.dial_attempts,
            );
            return outcome;
        }

        outcome.ip_unchanged = true;
        log::warn!(
            "Tcscloud IP unchanged after change window: server_id={server_id} server_name={server_name:?} round={round}/{max_rounds}"
        );
    }

    outcome.message = format!(
        "已换 IP {} 次，公网 IP 仍未变化，已停止自动重试",
        outcome.dial_attempts
    );
    outcome
}

async fn run_ssh(
    config: &TcscloudSeednetConfig,
    remote_command: &str,
) -> Result<String, TcscloudError> {
    let mut command = Command::new("ssh");
    command
        .kill_on_drop(true)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .args(ssh_args(config))
        .arg(remote_command);

    let output = timeout(SSH_TIMEOUT, command.output())
        .await
        .map_err(|_| TcscloudError::Timeout)?
        .map_err(|_| TcscloudError::SshUnavailable)?;
    if !output.status.success() {
        // stderr is only classified, never logged.
        return Err(classify_ssh_failure(&String::from_utf8_lossy(
            &output.stderr,
        )));
    }
    String::from_utf8(output.stdout).map_err(|_| TcscloudError::InvalidResponse)
}

fn ssh_args(config: &TcscloudSeednetConfig) -> Vec<String> {
    let mut args = vec![
        // Ignore ~/.ssh/config and /etc/ssh/ssh_config so nothing can relax the pinning.
        "-F".to_string(),
        "/dev/null".to_string(),
        "-i".to_string(),
        config.ssh_key_path.display().to_string(),
        "-p".to_string(),
        config.ssh_port.to_string(),
        "-l".to_string(),
        config.ssh_user.clone(),
    ];
    for option in [
        "BatchMode=yes".to_string(),
        "IdentitiesOnly=yes".to_string(),
        "ConnectTimeout=8".to_string(),
        "ConnectionAttempts=1".to_string(),
        "ServerAliveInterval=5".to_string(),
        "ServerAliveCountMax=2".to_string(),
        "StrictHostKeyChecking=yes".to_string(),
        format!("UserKnownHostsFile={}", config.known_hosts_path.display()),
        "GlobalKnownHostsFile=/dev/null".to_string(),
        format!("HostKeyAlias={}", config.host_key_alias()),
        "CheckHostIP=no".to_string(),
        "UpdateHostKeys=no".to_string(),
    ] {
        args.push("-o".to_string());
        args.push(option);
    }
    args.push("--".to_string());
    args.push(config.ssh_host.clone());
    args
}

fn classify_ssh_failure(stderr: &str) -> TcscloudError {
    let lower = stderr.to_ascii_lowercase();
    if lower.contains("host key verification failed")
        || lower.contains("remote host identification has changed")
        || lower.contains("host key for")
    {
        TcscloudError::HostKeyMismatch
    } else if lower.contains("timed out") {
        TcscloudError::Timeout
    } else {
        TcscloudError::SshUnavailable
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        collections::VecDeque,
        path::PathBuf,
        sync::{Arc, Mutex},
    };

    type GetResult = Result<IpAddr, TcscloudError>;
    type StartResult = Result<bool, TcscloudError>;

    #[derive(Clone)]
    struct MockTransport {
        ips: Arc<Mutex<VecDeque<GetResult>>>,
        starts: Arc<Mutex<VecDeque<StartResult>>>,
        start_count: Arc<Mutex<usize>>,
        get_count: Arc<Mutex<usize>>,
    }

    impl MockTransport {
        fn new(ips: Vec<GetResult>, starts: Vec<StartResult>) -> Self {
            Self {
                ips: Arc::new(Mutex::new(ips.into())),
                starts: Arc::new(Mutex::new(starts.into())),
                start_count: Arc::new(Mutex::new(0)),
                get_count: Arc::new(Mutex::new(0)),
            }
        }

        fn starts(&self) -> usize {
            *self.start_count.lock().unwrap()
        }

        fn gets(&self) -> usize {
            *self.get_count.lock().unwrap()
        }
    }

    impl TcscloudTransport for MockTransport {
        async fn get_ip(&self, _config: &TcscloudSeednetConfig) -> GetResult {
            *self.get_count.lock().unwrap() += 1;
            // Exhausted queue = box unreachable (keeps deadline tests simple).
            self.ips
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Err(TcscloudError::SshUnavailable))
        }

        async fn start_change(&self, _config: &TcscloudSeednetConfig) -> StartResult {
            *self.start_count.lock().unwrap() += 1;
            self.starts.lock().unwrap().pop_front().unwrap()
        }
    }

    fn config() -> TcscloudSeednetConfig {
        TcscloudSeednetConfig {
            ssh_host: "tcsseednet.example.com".to_string(),
            ssh_port: 22,
            ssh_user: "root".to_string(),
            ssh_key_path: PathBuf::from("/etc/boil/keys/tcs-test"),
            known_hosts_path: PathBuf::from("/etc/boil/keys/tcs-test.known_hosts"),
            host_key_alias: None,
        }
    }

    fn policy(attempts: usize) -> TcscloudReconnectPolicy {
        TcscloudReconnectPolicy {
            initial_wait: Duration::ZERO,
            poll_interval: Duration::ZERO,
            confirmation_window: Duration::from_secs(5),
            max_poll_attempts_per_round: attempts,
            max_rounds: 2,
        }
    }

    fn ip(value: &str) -> GetResult {
        Ok(value.parse().unwrap())
    }

    const OLD: &str = "175.182.177.190";
    const NEW: &str = "123.204.135.20";

    async fn run_with(
        mock: &MockTransport,
        policy: &TcscloudReconnectPolicy,
    ) -> TcscloudReconnectOutcome {
        let lock = tokio::sync::Mutex::new(());
        reconnect_with_transport(
            mock,
            &config(),
            "tcs-seednet-1",
            "Tcscloud Seednet",
            OLD.parse().unwrap(),
            policy,
            &lock,
        )
        .await
    }

    async fn run(mock: &MockTransport, attempts: usize) -> TcscloudReconnectOutcome {
        run_with(mock, &policy(attempts)).await
    }

    #[tokio::test]
    async fn new_ip_confirms_success_after_one_start() {
        let mock = MockTransport::new(vec![ip(NEW)], vec![Ok(true)]);
        let result = run(&mock, 5).await;
        assert!(result.changed);
        assert_eq!(result.new_ip, Some(NEW.parse().unwrap()));
        assert_eq!(result.dial_attempts, 1);
        assert_eq!(result.poll_attempts, 1);
        assert_eq!(mock.starts(), 1);
        assert_eq!(result.message, "换 IP 已完成");
    }

    #[tokio::test]
    async fn transient_changing_and_ssh_failures_keep_polling() {
        let mock = MockTransport::new(
            vec![
                Err(TcscloudError::Changing),
                Err(TcscloudError::SshUnavailable),
                Err(TcscloudError::HostKeyMismatch),
                Err(TcscloudError::Timeout),
                ip(OLD),
                ip(NEW),
            ],
            vec![Ok(true)],
        );
        let result = run(&mock, 10).await;
        assert!(result.changed);
        assert_eq!(result.new_ip, Some(NEW.parse().unwrap()));
        assert_eq!(result.poll_attempts, 6);
        assert_eq!(mock.starts(), 1);
    }

    #[tokio::test]
    async fn unreachable_until_deadline_is_unconfirmed_without_second_start() {
        let mock = MockTransport::new(vec![], vec![Ok(true)]);
        let mut test_policy = policy(usize::MAX);
        test_policy.confirmation_window = Duration::from_millis(60);
        test_policy.poll_interval = Duration::from_millis(10);
        let result = run_with(&mock, &test_policy).await;
        assert!(!result.changed);
        assert!(result.change_requested);
        assert!(!result.ip_unchanged);
        assert_eq!(result.new_ip, None);
        assert_eq!(mock.starts(), 1);
        assert!(mock.gets() >= 2);
        assert!(result.message.contains("未能确认新 IP"));
    }

    #[tokio::test]
    async fn changing_at_deadline_is_unconfirmed_without_second_start() {
        let mock = MockTransport::new(vec![ip(OLD), Err(TcscloudError::Changing)], vec![Ok(true)]);
        let result = run(&mock, 2).await;
        assert!(!result.changed);
        assert!(result.change_requested);
        assert!(!result.ip_unchanged);
        assert_eq!(mock.starts(), 1);
    }

    #[tokio::test]
    async fn same_ip_at_deadline_starts_one_second_round() {
        let mock = MockTransport::new(vec![ip(OLD), ip(OLD), ip(NEW)], vec![Ok(true), Ok(true)]);
        let result = run(&mock, 2).await;
        assert!(result.changed);
        assert_eq!(result.dial_attempts, 2);
        assert_eq!(mock.starts(), 2);
        assert_eq!(result.new_ip, Some(NEW.parse().unwrap()));
    }

    #[tokio::test]
    async fn same_ip_twice_stops_after_hard_limit() {
        let mock = MockTransport::new(
            vec![ip(OLD), ip(OLD), ip(OLD), ip(OLD)],
            vec![Ok(true), Ok(true), Ok(true)],
        );
        let mut test_policy = policy(2);
        test_policy.max_rounds = 99;
        let result = run_with(&mock, &test_policy).await;
        assert!(!result.changed);
        assert!(result.ip_unchanged);
        assert_eq!(result.dial_attempts, 2);
        assert_eq!(mock.starts(), 2);
        assert!(result.message.contains("仍未变化"));
    }

    #[tokio::test]
    async fn old_ip_then_unreachable_does_not_count_as_same_ip() {
        let mock = MockTransport::new(
            vec![ip(OLD), Err(TcscloudError::HostKeyMismatch)],
            vec![Ok(true), Ok(true)],
        );
        let result = run(&mock, 2).await;
        assert!(!result.changed);
        assert!(!result.ip_unchanged);
        assert_eq!(mock.starts(), 1);
    }

    #[tokio::test]
    async fn host_key_mismatch_or_ssh_failure_at_start_fails_without_polling() {
        for error in [
            TcscloudError::HostKeyMismatch,
            TcscloudError::SshUnavailable,
        ] {
            let mock = MockTransport::new(vec![ip(NEW)], vec![Err(error.clone())]);
            let result = run(&mock, 5).await;
            assert!(!result.changed);
            assert!(!result.change_requested);
            assert_eq!(result.poll_attempts, 0);
            assert_eq!(mock.gets(), 0);
            assert_eq!(mock.starts(), 1);
            assert!(result.message.starts_with("未能发起换 IP"));
        }
    }

    #[tokio::test]
    async fn ambiguous_start_still_confirms() {
        for start in [Ok(false), Err(TcscloudError::Timeout)] {
            let mock = MockTransport::new(vec![ip(NEW)], vec![start]);
            let result = run(&mock, 3).await;
            assert!(result.changed);
            assert!(result.change_requested);
            assert_eq!(mock.starts(), 1);
        }
    }

    #[tokio::test]
    async fn second_round_start_failure_reports_both_rounds() {
        let mock = MockTransport::new(
            vec![ip(OLD)],
            vec![Ok(true), Err(TcscloudError::SshUnavailable)],
        );
        let result = run(&mock, 1).await;
        assert!(!result.changed);
        assert!(result.ip_unchanged);
        assert_eq!(result.dial_attempts, 2);
        assert!(result.message.contains("第 2 次未能发起"));
    }

    #[test]
    fn default_policy_waits_then_polls_for_five_minutes() {
        let policy = TcscloudReconnectPolicy::default();
        assert_eq!(policy.initial_wait, Duration::from_secs(30));
        assert_eq!(policy.poll_interval, Duration::from_secs(10));
        assert_eq!(policy.confirmation_window, Duration::from_secs(300));
        assert_eq!(policy.max_poll_attempts_per_round, usize::MAX);
        assert_eq!(policy.max_rounds, 2);
    }

    fn output(vendor: &str, public: &str) -> String {
        format!("{vendor}\n{PUBLIC_IP_MARKER}\n{public}")
    }

    fn resolve(vendor: &str, public: &str) -> Result<IpAddr, TcscloudError> {
        resolve_snapshot(&parse_get_ip_output(&output(vendor, public))?)
    }

    #[test]
    fn get_ip_parsing_handles_vendor_and_egress_views() {
        let ok = format!(r#"{{"status":"success","action":"get_ip","ip":"{NEW}"}}"#);
        let empty = r#"{"status":"success","action":"get_ip","ip":""}"#;
        assert_eq!(resolve(&ok, NEW), ip(NEW));
        // Egress check failed: trust the vendor view.
        assert_eq!(resolve(&ok, ""), ip(NEW));
        // Empty vendor IP = change in progress, whatever the egress says.
        assert_eq!(resolve(empty, OLD), Err(TcscloudError::Changing));
        assert_eq!(resolve(empty, ""), Err(TcscloudError::Changing));
        // Vendor API down: egress IP is enough.
        assert_eq!(resolve("", NEW), ip(NEW));
        assert_eq!(resolve("curl: (6) Could not resolve host", NEW), ip(NEW));
        // Mismatch: egress is the ground truth.
        assert_eq!(resolve(&ok, OLD), ip(OLD));
        // Nothing usable.
        assert_eq!(resolve("", ""), Err(TcscloudError::InvalidPublicIpv4));
        assert_eq!(
            resolve(r#"{"status":"failed","ip":"1.2.3.4"}"#, "10.0.0.1"),
            Err(TcscloudError::InvalidPublicIpv4)
        );
        assert_eq!(
            resolve(r#"{"status":"success","ip":"198.18.2.103"}"#, ""),
            Err(TcscloudError::InvalidPublicIpv4)
        );
        assert_eq!(
            parse_get_ip_output("no marker here"),
            Err(TcscloudError::InvalidResponse)
        );
    }

    #[test]
    fn ssh_args_pin_host_key_strictly() {
        let args = ssh_args(&config());
        let joined = args.join(" ");
        assert!(joined.starts_with("-F /dev/null "));
        assert!(joined.contains("-o StrictHostKeyChecking=yes"));
        assert!(joined.contains("-o UserKnownHostsFile=/etc/boil/keys/tcs-test.known_hosts"));
        assert!(joined.contains("-o GlobalKnownHostsFile=/dev/null"));
        assert!(joined.contains("-o HostKeyAlias=tcsseednet.example.com"));
        assert!(joined.contains("-o BatchMode=yes"));
        assert!(joined.contains("-o UpdateHostKeys=no"));
        assert!(!joined.contains("accept-new"));
        assert_eq!(
            &args[args.len() - 2..],
            ["--".to_string(), "tcsseednet.example.com".to_string()]
        );

        let mut aliased = config();
        aliased.host_key_alias = Some("pinned.example.com".to_string());
        assert!(ssh_args(&aliased)
            .join(" ")
            .contains("-o HostKeyAlias=pinned.example.com"));
    }

    #[test]
    fn ssh_failures_are_classified_without_exposing_stderr() {
        assert_eq!(
            classify_ssh_failure(
                "@@@ WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED! @@@\r\nHost key verification failed.\r\n"
            ),
            TcscloudError::HostKeyMismatch
        );
        assert_eq!(
            classify_ssh_failure("No ED25519 host key is known for x and you have requested strict checking.\r\nHost key verification failed.\r\n"),
            TcscloudError::HostKeyMismatch
        );
        assert_eq!(
            classify_ssh_failure("ssh: connect to host x port 22: Connection timed out\r\n"),
            TcscloudError::Timeout
        );
        assert_eq!(
            classify_ssh_failure("ssh: connect to host x port 22: Connection refused\r\n"),
            TcscloudError::SshUnavailable
        );
    }

    #[test]
    fn remote_commands_are_fixed_and_detached() {
        assert!(CHANGE_START_COMMAND.contains("setsid nohup"));
        assert!(CHANGE_START_COMMAND.contains("flock -n"));
        assert!(CHANGE_START_COMMAND.contains("</dev/null &"));
        assert!(CHANGE_START_COMMAND.ends_with(CHANGE_STARTED_MARKER));
        assert!(CHANGE_START_COMMAND.contains("http://ipapi.tcscloud.net:8002/changeip"));
        assert!(!GET_IP_COMMAND.contains("changeip"));
        assert!(GET_IP_COMMAND.contains(PUBLIC_IP_MARKER));
    }
}
