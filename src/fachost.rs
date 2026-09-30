use std::{
    future::Future,
    net::{IpAddr, Ipv4Addr},
    process::Stdio,
    time::Duration,
};

use serde::Deserialize;
use tokio::{process::Command, time::timeout};

use crate::config::FacHostSeednetConfig;

const PUBLIC_IP_COMMAND: &str = r#"for url in https://api.ipify.org https://ipv4.icanhazip.com; do value=$(curl -4fsS --connect-timeout 5 --max-time 8 "$url") && printf '%s' "$value" && exit 0; done; exit 1"#;
const REDIAL_HTTP_STATUS_MARKER: &str = "__BOIL_HTTP_STATUS__:";
const REDIAL_COMMAND: &str = "curl -sS --connect-timeout 5 --max-time 15 -w '\n__BOIL_HTTP_STATUS__:%{http_code}' http://192.168.11.213:8080/re-pppoe";
const SSH_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FacHostReconnectPolicy {
    pub poll_interval: Duration,
    pub confirmation_window: Duration,
    pub max_poll_attempts_per_round: usize,
    pub max_rounds: usize,
}

impl Default for FacHostReconnectPolicy {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(5),
            confirmation_window: Duration::from_secs(120),
            // Production is deadline-driven. Tests set a finite cap without waiting 120 seconds.
            max_poll_attempts_per_round: usize::MAX,
            max_rounds: 2,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FacHostReconnectOutcome {
    pub old_ip: IpAddr,
    pub new_ip: Option<IpAddr>,
    pub changed: bool,
    pub provider_confirmed_success: bool,
    pub dial_attempts: usize,
    pub poll_attempts: usize,
    pub provider_dial_count: Option<u32>,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FacHostError {
    SshUnavailable,
    Timeout,
    InvalidPublicIpv4,
    InvalidResponse,
}

impl std::fmt::Display for FacHostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::SshUnavailable => "无法通过 SSH 查询 Seednet VPS",
            Self::Timeout => "Seednet VPS 查询超时",
            Self::InvalidPublicIpv4 => "Seednet VPS 返回的公网 IPv4 无效",
            Self::InvalidResponse => "Seednet VPS 返回了无效响应",
        };
        f.write_str(message)
    }
}

impl std::error::Error for FacHostError {}

#[derive(Clone, Debug, Default)]
pub struct FacHostSeednetClient;

impl FacHostSeednetClient {
    pub async fn get_ip(&self, config: &FacHostSeednetConfig) -> Result<IpAddr, FacHostError> {
        SshTransport.get_ip(config).await
    }

    pub async fn reconnect<F, Fut>(
        &self,
        config: &FacHostSeednetConfig,
        server_id: &str,
        server_name: &str,
        old_ip: IpAddr,
        policy: &FacHostReconnectPolicy,
        on_provider_confirmed: F,
    ) -> FacHostReconnectOutcome
    where
        F: FnMut(IpAddr, IpAddr) -> Fut,
        Fut: Future<Output = ()>,
    {
        reconnect_with_transport(
            &SshTransport,
            config,
            server_id,
            server_name,
            old_ip,
            policy,
            on_provider_confirmed,
        )
        .await
    }
}

trait FacHostTransport: Send + Sync {
    fn get_ip(
        &self,
        config: &FacHostSeednetConfig,
    ) -> impl Future<Output = Result<IpAddr, FacHostError>> + Send;

    fn redial(
        &self,
        config: &FacHostSeednetConfig,
    ) -> impl Future<Output = Result<RedialTransportResponse, FacHostError>> + Send;
}

struct SshTransport;

impl FacHostTransport for SshTransport {
    async fn get_ip(&self, config: &FacHostSeednetConfig) -> Result<IpAddr, FacHostError> {
        let output = run_ssh(config, PUBLIC_IP_COMMAND).await?;
        let ip: Ipv4Addr = output
            .trim()
            .parse()
            .map_err(|_| FacHostError::InvalidPublicIpv4)?;
        if !is_public_ipv4(ip) {
            return Err(FacHostError::InvalidPublicIpv4);
        }
        Ok(IpAddr::V4(ip))
    }

    async fn redial(
        &self,
        config: &FacHostSeednetConfig,
    ) -> Result<RedialTransportResponse, FacHostError> {
        let output = run_ssh(config, REDIAL_COMMAND).await?;
        Ok(parse_redial_transport_output(output))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RedialTransportResponse {
    http_status: Option<u16>,
    body: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
struct RedialResponse {
    #[serde(default)]
    changed: Option<bool>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    ip: Option<String>,
    #[serde(default)]
    old_ip: Option<String>,
    #[serde(default)]
    dial_count: Option<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RedialDisposition {
    Confirmed(IpAddr),
    ExplicitFailure,
    Unknown,
}

fn parse_redial_transport_output(output: String) -> RedialTransportResponse {
    let trimmed = output.trim_end();
    let Some((body, status)) = trimmed.rsplit_once(REDIAL_HTTP_STATUS_MARKER) else {
        return RedialTransportResponse {
            http_status: None,
            body: trimmed.to_string(),
        };
    };
    RedialTransportResponse {
        http_status: status.trim().parse().ok(),
        body: body.trim_end().to_string(),
    }
}

fn classify_redial_response(
    http_status: Option<u16>,
    response: &RedialResponse,
    old_ip: IpAddr,
) -> RedialDisposition {
    let normalized_status = response
        .status
        .as_deref()
        .map(str::trim)
        .map(str::to_ascii_lowercase);
    let explicit_failure = response.changed == Some(false)
        || matches!(
            normalized_status.as_deref(),
            Some("failed" | "failure" | "error")
        );
    if explicit_failure {
        return RedialDisposition::ExplicitFailure;
    }

    let Some(ip) = response
        .ip
        .as_deref()
        .and_then(|value| value.trim().parse::<Ipv4Addr>().ok())
        .filter(|ip| is_public_ipv4(*ip))
        .map(IpAddr::V4)
    else {
        return RedialDisposition::Unknown;
    };

    if matches!(http_status, Some(200..=299))
        && normalized_status.as_deref() == Some("success")
        && response.changed == Some(true)
        && ip != old_ip
    {
        RedialDisposition::Confirmed(ip)
    } else {
        RedialDisposition::Unknown
    }
}

fn safe_status_for_log(status: Option<&str>) -> &'static str {
    match status
        .map(str::trim)
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("success") => "success",
        Some("failed") => "failed",
        Some("failure") => "failure",
        Some("error") => "error",
        Some(_) => "other",
        None => "missing",
    }
}

fn parsed_ip_for_log(value: Option<&str>) -> Option<IpAddr> {
    value.and_then(|value| value.trim().parse().ok())
}

async fn reconnect_with_transport<T, F, Fut>(
    transport: &T,
    config: &FacHostSeednetConfig,
    server_id: &str,
    server_name: &str,
    old_ip: IpAddr,
    policy: &FacHostReconnectPolicy,
    mut on_provider_confirmed: F,
) -> FacHostReconnectOutcome
where
    T: FacHostTransport,
    F: FnMut(IpAddr, IpAddr) -> Fut,
    Fut: Future<Output = ()>,
{
    let mut outcome = FacHostReconnectOutcome {
        old_ip,
        new_ip: None,
        changed: false,
        provider_confirmed_success: false,
        dial_attempts: 0,
        poll_attempts: 0,
        provider_dial_count: None,
        message: "换 IP 后无法确认新的公网 IPv4".to_string(),
    };

    let max_rounds = policy.max_rounds.min(2);
    for round_index in 0..max_rounds {
        let round = round_index + 1;
        outcome.dial_attempts += 1;
        log::info!(
            "FACHost redial started: server_id={server_id} server_name={server_name:?} round={round}/{max_rounds}"
        );

        let provider_confirmed_ip = match transport.redial(config).await {
            Ok(transport_response) => {
                let http_status = transport_response.http_status;
                match serde_json::from_str::<RedialResponse>(&transport_response.body) {
                    Ok(response) => {
                        outcome.provider_dial_count =
                            response.dial_count.or(outcome.provider_dial_count);
                        let disposition = classify_redial_response(http_status, &response, old_ip);
                        let result_class = match disposition {
                            RedialDisposition::Confirmed(_) => "success",
                            RedialDisposition::ExplicitFailure => "explicit_failure",
                            RedialDisposition::Unknown => "unknown",
                        };
                        log::info!(
                            "FACHost redial response: server_id={server_id} server_name={server_name:?} round={round} result={result_class} http_status={http_status:?} json_changed={:?} json_status={} json_old_ip={:?} json_ip={:?} json_dial_count={:?} provider_confirmed_success={}",
                            response.changed,
                            safe_status_for_log(response.status.as_deref()),
                            parsed_ip_for_log(response.old_ip.as_deref()),
                            parsed_ip_for_log(response.ip.as_deref()),
                            response.dial_count,
                            matches!(disposition, RedialDisposition::Confirmed(_)),
                        );
                        match disposition {
                            RedialDisposition::Confirmed(ip) => Some(ip),
                            RedialDisposition::ExplicitFailure | RedialDisposition::Unknown => None,
                        }
                    }
                    Err(_) => {
                        log::warn!(
                            "FACHost redial response: server_id={server_id} server_name={server_name:?} round={round} result=parse_error http_status={http_status:?} provider_confirmed_success=false"
                        );
                        None
                    }
                }
            }
            Err(error) => {
                let result_class = match error {
                    FacHostError::Timeout => "timeout",
                    FacHostError::SshUnavailable => "disconnect_or_transport_error",
                    FacHostError::InvalidPublicIpv4 | FacHostError::InvalidResponse => {
                        "invalid_transport_response"
                    }
                };
                log::warn!(
                    "FACHost redial transport failed: server_id={server_id} server_name={server_name:?} round={round} result={result_class} provider_confirmed_success=false error={error}"
                );
                None
            }
        };

        if let Some(confirmed_ip) = provider_confirmed_ip {
            outcome.provider_confirmed_success = true;
            outcome.new_ip = Some(confirmed_ip);
            outcome.message =
                "FACHost 已明确返回换 IP 成功，但暂时无法确认目标 VPS 网络恢复".to_string();
            on_provider_confirmed(old_ip, confirmed_ip).await;
        }

        let confirmation_started = tokio::time::Instant::now();
        let deadline = confirmation_started + policy.confirmation_window;
        let mut round_poll_attempts = 0usize;
        loop {
            if round_poll_attempts >= policy.max_poll_attempts_per_round {
                log::info!(
                    "FACHost confirmation test cap reached: server_id={server_id} server_name={server_name:?} round={round} polls={round_poll_attempts}"
                );
                break;
            }
            if round_poll_attempts > 0 {
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
            round_poll_attempts += 1;
            outcome.poll_attempts += 1;
            let elapsed = confirmation_started.elapsed().as_secs();
            match timeout(remaining, transport.get_ip(config)).await {
                Ok(Ok(current_ip)) => {
                    let changed = current_ip != old_ip;
                    log::info!(
                        "FACHost confirmation poll: server_id={server_id} server_name={server_name:?} round={round} poll={round_poll_attempts} current_ip={current_ip} changed={changed} elapsed_seconds={elapsed}"
                    );
                    if changed {
                        outcome.new_ip = Some(current_ip);
                        outcome.changed = true;
                        outcome.message = "换 IP 已完成".to_string();
                        log::info!(
                            "FACHost reconnect completed: server_id={server_id} server_name={server_name:?} changed=true provider_confirmed_success={} dial_attempts={} final_ip={current_ip}",
                            outcome.provider_confirmed_success,
                            outcome.dial_attempts,
                        );
                        return outcome;
                    } else if !outcome.provider_confirmed_success {
                        outcome.new_ip = Some(current_ip);
                    }
                }
                Ok(Err(error)) => {
                    log::warn!(
                        "FACHost confirmation poll failed: server_id={server_id} server_name={server_name:?} round={round} poll={round_poll_attempts} elapsed_seconds={elapsed} error={error}"
                    );
                }
                Err(_) => {
                    log::warn!(
                        "FACHost confirmation poll timed out at deadline: server_id={server_id} server_name={server_name:?} round={round} poll={round_poll_attempts} elapsed_seconds={elapsed}"
                    );
                    break;
                }
            }
        }

        let allow_second_round = !outcome.provider_confirmed_success && round < max_rounds;
        log::warn!(
            "FACHost confirmation deadline reached: server_id={server_id} server_name={server_name:?} round={round} provider_confirmed_success={} allow_second_round={allow_second_round} dial_attempts={}",
            outcome.provider_confirmed_success,
            outcome.dial_attempts,
        );
        if outcome.provider_confirmed_success {
            log::warn!(
                "FACHost reconnect completed without network confirmation: server_id={server_id} server_name={server_name:?} dial_attempts={} provider_ip={:?}",
                outcome.dial_attempts,
                outcome.new_ip,
            );
            return outcome;
        }
    }

    log::warn!(
        "FACHost reconnect failed after hard round limit: server_id={server_id} server_name={server_name:?} dial_attempts={} poll_attempts={}",
        outcome.dial_attempts,
        outcome.poll_attempts,
    );
    outcome
}

async fn run_ssh(
    config: &FacHostSeednetConfig,
    remote_command: &str,
) -> Result<String, FacHostError> {
    let destination = format!("{}@{}", config.ssh_user, config.ssh_host);
    let mut command = Command::new("ssh");
    command
        .kill_on_drop(true)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .arg("-i")
        .arg(&config.ssh_key_path)
        .arg("-p")
        .arg(config.ssh_port.to_string())
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg("ConnectTimeout=8")
        .arg("-o")
        .arg("ConnectionAttempts=1")
        .arg("-o")
        .arg("StrictHostKeyChecking=accept-new")
        .arg(destination)
        .arg(remote_command);

    let output = timeout(SSH_TIMEOUT, command.output())
        .await
        .map_err(|_| FacHostError::Timeout)?
        .map_err(|_| FacHostError::SshUnavailable)?;
    if !output.status.success() {
        if output.status.code() == Some(28) {
            return Err(FacHostError::Timeout);
        }
        return Err(FacHostError::SshUnavailable);
    }
    String::from_utf8(output.stdout).map_err(|_| FacHostError::InvalidResponse)
}

fn is_public_ipv4(ip: Ipv4Addr) -> bool {
    let [a, b, _, _] = ip.octets();
    !ip.is_private()
        && !ip.is_loopback()
        && !ip.is_link_local()
        && !ip.is_multicast()
        && !ip.is_unspecified()
        && ip != Ipv4Addr::BROADCAST
        && a != 0
        && !(a == 100 && (64..=127).contains(&b))
        && !(a == 192 && b == 0)
        && !(a == 198 && b == 51 && ip.octets()[2] == 100)
        && !(a == 203 && b == 0 && ip.octets()[2] == 113)
        && !(a == 198 && (b == 18 || b == 19))
        && a < 224
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        collections::VecDeque,
        path::PathBuf,
        sync::{Arc, Mutex},
    };

    type RedialResult = Result<RedialTransportResponse, FacHostError>;

    #[derive(Clone)]
    struct MockTransport {
        ips: Arc<Mutex<VecDeque<Result<IpAddr, FacHostError>>>>,
        redials: Arc<Mutex<VecDeque<RedialResult>>>,
        redial_count: Arc<Mutex<usize>>,
    }

    impl MockTransport {
        fn new(ips: Vec<Result<IpAddr, FacHostError>>, redials: Vec<RedialResult>) -> Self {
            Self {
                ips: Arc::new(Mutex::new(ips.into())),
                redials: Arc::new(Mutex::new(redials.into())),
                redial_count: Arc::new(Mutex::new(0)),
            }
        }

        fn calls(&self) -> usize {
            *self.redial_count.lock().unwrap()
        }
    }

    impl FacHostTransport for MockTransport {
        async fn get_ip(&self, _config: &FacHostSeednetConfig) -> Result<IpAddr, FacHostError> {
            self.ips.lock().unwrap().pop_front().unwrap()
        }

        async fn redial(
            &self,
            _config: &FacHostSeednetConfig,
        ) -> Result<RedialTransportResponse, FacHostError> {
            *self.redial_count.lock().unwrap() += 1;
            self.redials.lock().unwrap().pop_front().unwrap()
        }
    }

    fn config() -> FacHostSeednetConfig {
        FacHostSeednetConfig {
            ssh_host: "seednet.example.com".to_string(),
            ssh_port: 22,
            ssh_user: "root".to_string(),
            ssh_key_path: PathBuf::from("/tmp/test-key"),
        }
    }

    fn policy(attempts: usize) -> FacHostReconnectPolicy {
        FacHostReconnectPolicy {
            poll_interval: Duration::ZERO,
            confirmation_window: Duration::from_secs(1),
            max_poll_attempts_per_round: attempts,
            max_rounds: 2,
        }
    }

    fn ip(value: &str) -> Result<IpAddr, FacHostError> {
        Ok(value.parse().unwrap())
    }

    fn response(body: &str) -> RedialResult {
        Ok(RedialTransportResponse {
            http_status: Some(200),
            body: body.to_string(),
        })
    }

    fn confirmed(new_ip: &str, dial_count: u32) -> RedialResult {
        response(&format!(
            r#"{{"changed":true,"dial_count":{dial_count},"ip":"{new_ip}","old_ip":"8.8.8.8","status":"success","wan":"wan1"}}"#
        ))
    }

    fn failed(dial_count: u32) -> RedialResult {
        response(&format!(
            r#"{{"changed":false,"dial_count":{dial_count},"status":"failed"}}"#
        ))
    }

    async fn run(mock: &MockTransport, attempts: usize) -> FacHostReconnectOutcome {
        reconnect_with_transport(
            mock,
            &config(),
            "seednet-1",
            "Seednet VPS",
            "8.8.8.8".parse().unwrap(),
            &policy(attempts),
            |_, _| async {},
        )
        .await
    }

    #[tokio::test]
    async fn confirmed_success_never_starts_second_round() {
        let mock = MockTransport::new(vec![ip("1.1.1.1")], vec![confirmed("8.8.4.4", 2)]);
        let result = run(&mock, 3).await;
        assert!(result.changed);
        assert!(result.provider_confirmed_success);
        assert_eq!(result.new_ip, Some("1.1.1.1".parse().unwrap()));
        assert_eq!(result.dial_attempts, 1);
        assert_eq!(result.provider_dial_count, Some(2));
        assert_eq!(mock.calls(), 1);
    }

    #[tokio::test]
    async fn confirmed_success_without_network_recovery_is_unconfirmed_after_one_call() {
        let mock = MockTransport::new(
            vec![
                Err(FacHostError::SshUnavailable),
                Err(FacHostError::Timeout),
                ip("8.8.8.8"),
            ],
            vec![confirmed("8.8.4.4", 1)],
        );
        let result = run(&mock, 3).await;
        assert!(!result.changed);
        assert!(result.provider_confirmed_success);
        assert_eq!(result.new_ip, Some("8.8.4.4".parse().unwrap()));
        assert_eq!(result.dial_attempts, 1);
        assert_eq!(mock.calls(), 1);
    }

    #[tokio::test]
    async fn confirmed_success_with_stale_dns_never_starts_second_round() {
        let mock = MockTransport::new(
            vec![ip("8.8.8.8"), ip("8.8.8.8")],
            vec![confirmed("8.8.4.4", 1)],
        );
        let result = run(&mock, 2).await;
        assert!(!result.changed);
        assert!(result.provider_confirmed_success);
        assert_eq!(mock.calls(), 1);
    }

    #[tokio::test]
    async fn timeout_or_disconnect_then_ip_change_succeeds_in_first_round() {
        for redial in [
            Err(FacHostError::Timeout),
            Err(FacHostError::SshUnavailable),
        ] {
            let mock = MockTransport::new(vec![ip("8.8.4.4")], vec![redial]);
            let result = run(&mock, 1).await;
            assert!(result.changed);
            assert!(!result.provider_confirmed_success);
            assert_eq!(mock.calls(), 1);
        }
    }

    #[tokio::test]
    async fn malformed_json_then_ip_change_succeeds_in_first_round() {
        let mock = MockTransport::new(vec![ip("8.8.4.4")], vec![response("{broken")]);
        let result = run(&mock, 1).await;
        assert!(result.changed);
        assert_eq!(mock.calls(), 1);
    }

    #[tokio::test]
    async fn explicit_failure_still_polls_and_accepts_ip_change() {
        let mock = MockTransport::new(vec![ip("8.8.8.8"), ip("8.8.4.4")], vec![failed(1)]);
        let result = run(&mock, 2).await;
        assert!(result.changed);
        assert_eq!(result.dial_attempts, 1);
        assert_eq!(mock.calls(), 1);
    }

    #[tokio::test]
    async fn unknown_result_waits_full_test_window_before_second_round() {
        let mock = MockTransport::new(
            vec![ip("8.8.8.8"), ip("8.8.8.8"), ip("8.8.4.4")],
            vec![response(""), confirmed("8.8.4.4", 2)],
        );
        let result = run(&mock, 2).await;
        assert!(result.changed);
        assert_eq!(result.poll_attempts, 3);
        assert_eq!(result.dial_attempts, 2);
        assert_eq!(mock.calls(), 2);
    }

    #[tokio::test]
    async fn explicit_failure_waits_full_test_window_before_second_round() {
        let mock = MockTransport::new(
            vec![ip("8.8.8.8"), ip("8.8.8.8"), ip("8.8.4.4")],
            vec![failed(1), confirmed("8.8.4.4", 2)],
        );
        let result = run(&mock, 2).await;
        assert!(result.changed);
        assert_eq!(result.poll_attempts, 3);
        assert_eq!(result.dial_attempts, 2);
        assert_eq!(mock.calls(), 2);
    }

    #[tokio::test]
    async fn second_round_confirmed_success_never_starts_third_round() {
        let mock = MockTransport::new(
            vec![ip("8.8.8.8"), ip("8.8.8.8"), ip("8.8.8.8"), ip("8.8.8.8")],
            vec![response(""), confirmed("8.8.4.4", 2)],
        );
        let result = run(&mock, 2).await;
        assert!(!result.changed);
        assert!(result.provider_confirmed_success);
        assert_eq!(result.dial_attempts, 2);
        assert_eq!(mock.calls(), 2);
    }

    #[tokio::test]
    async fn two_unknown_rounds_fail_without_third_redial() {
        let mock = MockTransport::new(
            vec![ip("8.8.8.8"), ip("8.8.8.8"), ip("8.8.8.8"), ip("8.8.8.8")],
            vec![response(""), response("{broken")],
        );
        let result = run(&mock, 2).await;
        assert!(!result.changed);
        assert!(!result.provider_confirmed_success);
        assert_eq!(result.dial_attempts, 2);
        assert_eq!(mock.calls(), 2);
    }

    #[tokio::test]
    async fn hard_round_limit_is_two_even_if_policy_is_higher() {
        let mut test_policy = policy(1);
        test_policy.max_rounds = 99;
        let mock = MockTransport::new(
            vec![ip("8.8.8.8"), ip("8.8.8.8")],
            vec![response(""), response(""), response("")],
        );
        let result = reconnect_with_transport(
            &mock,
            &config(),
            "seednet-1",
            "Seednet VPS",
            "8.8.8.8".parse().unwrap(),
            &test_policy,
            |_, _| async {},
        )
        .await;
        assert_eq!(result.dial_attempts, 2);
        assert_eq!(mock.calls(), 2);
    }

    #[test]
    fn parses_http_status_without_exposing_body() {
        let parsed = parse_redial_transport_output(
            "{\"status\":\"success\"}\n__BOIL_HTTP_STATUS__:200".to_string(),
        );
        assert_eq!(parsed.http_status, Some(200));
        assert_eq!(parsed.body, "{\"status\":\"success\"}");
    }

    #[test]
    fn default_policy_uses_deadline_driven_two_minute_confirmation() {
        let policy = FacHostReconnectPolicy::default();
        assert_eq!(policy.poll_interval, Duration::from_secs(5));
        assert_eq!(policy.confirmation_window, Duration::from_secs(120));
        assert_eq!(policy.max_poll_attempts_per_round, usize::MAX);
        assert_eq!(policy.max_rounds, 2);
    }

    #[test]
    fn strict_success_requires_all_provider_evidence() {
        let old_ip = "8.8.8.8".parse().unwrap();
        let valid = RedialResponse {
            changed: Some(true),
            status: Some("success".to_string()),
            ip: Some("8.8.4.4".to_string()),
            old_ip: Some("8.8.8.8".to_string()),
            dial_count: Some(1),
        };
        assert_eq!(
            classify_redial_response(Some(200), &valid, old_ip),
            RedialDisposition::Confirmed("8.8.4.4".parse().unwrap())
        );
        assert_eq!(
            classify_redial_response(Some(500), &valid, old_ip),
            RedialDisposition::Unknown
        );
        assert_eq!(
            classify_redial_response(
                Some(200),
                &RedialResponse {
                    ip: Some("192.168.1.2".to_string()),
                    ..valid
                },
                old_ip
            ),
            RedialDisposition::Unknown
        );
    }

    #[test]
    fn rejects_private_or_non_ipv4_addresses() {
        assert!(!is_public_ipv4("192.168.11.213".parse().unwrap()));
        assert!(!is_public_ipv4("100.64.1.1".parse().unwrap()));
        assert!(is_public_ipv4("112.104.136.174".parse().unwrap()));
    }
}
