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
const REDIAL_COMMAND: &str =
    "curl -fsS --connect-timeout 5 --max-time 15 http://192.168.11.213:8080/re-pppoe";
const SSH_TIMEOUT: Duration = Duration::from_secs(20);
pub const DAILY_DIAL_LIMIT: u32 = 5;

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
            confirmation_window: Duration::from_secs(60),
            max_poll_attempts_per_round: 12,
            max_rounds: 2,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FacHostReconnectOutcome {
    pub old_ip: IpAddr,
    pub new_ip: Option<IpAddr>,
    pub changed: bool,
    pub dial_attempts: usize,
    pub poll_attempts: usize,
    pub daily_dial_count: Option<u32>,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FacHostError {
    SshUnavailable,
    Timeout,
    InvalidPublicIpv4,
}

impl std::fmt::Display for FacHostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::SshUnavailable => "无法通过 SSH 查询 Seednet VPS",
            Self::Timeout => "Seednet VPS 查询超时",
            Self::InvalidPublicIpv4 => "Seednet VPS 返回的公网 IPv4 无效",
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

    pub async fn reconnect(
        &self,
        config: &FacHostSeednetConfig,
        old_ip: IpAddr,
        policy: &FacHostReconnectPolicy,
    ) -> FacHostReconnectOutcome {
        reconnect_with_transport(&SshTransport, config, old_ip, policy).await
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
    ) -> impl Future<Output = Result<Option<RedialResponse>, FacHostError>> + Send;
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
    ) -> Result<Option<RedialResponse>, FacHostError> {
        let output = run_ssh(config, REDIAL_COMMAND).await?;
        Ok(serde_json::from_str(&output).ok())
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
struct RedialResponse {
    dial_count: Option<u32>,
}

async fn reconnect_with_transport<T: FacHostTransport>(
    transport: &T,
    config: &FacHostSeednetConfig,
    old_ip: IpAddr,
    policy: &FacHostReconnectPolicy,
) -> FacHostReconnectOutcome {
    let mut outcome = FacHostReconnectOutcome {
        old_ip,
        new_ip: None,
        changed: false,
        dial_attempts: 0,
        poll_attempts: 0,
        daily_dial_count: None,
        message: "换 IP 后无法确认新的公网 IPv4".to_string(),
    };

    for _round in 0..policy.max_rounds.min(2) {
        outcome.dial_attempts += 1;
        if let Ok(Some(response)) = transport.redial(config).await {
            if let Some(count) = response.dial_count {
                outcome.daily_dial_count = Some(count);
            }
        }

        let deadline = tokio::time::Instant::now() + policy.confirmation_window;
        for attempt in 0..policy.max_poll_attempts_per_round {
            if attempt > 0 {
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
            outcome.poll_attempts += 1;
            match timeout(remaining, transport.get_ip(config)).await {
                Ok(Ok(current_ip)) => {
                    outcome.new_ip = Some(current_ip);
                    if current_ip != old_ip {
                        outcome.changed = true;
                        outcome.message = "换 IP 已完成".to_string();
                        return outcome;
                    }
                }
                Ok(Err(error)) => {
                    log::debug!(
                        "Seednet 换 IP 后公网 IPv4 暂不可用: attempt={}: {error}",
                        outcome.poll_attempts
                    );
                }
                Err(_) => {
                    log::debug!(
                        "Seednet 换 IP 后公网 IPv4 查询超过本轮确认窗口: attempt={}",
                        outcome.poll_attempts
                    );
                    break;
                }
            }
        }
    }

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
        return Err(FacHostError::SshUnavailable);
    }
    String::from_utf8(output.stdout).map_err(|_| FacHostError::InvalidPublicIpv4)
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

    type RedialResult = Result<Option<RedialResponse>, FacHostError>;

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
        ) -> Result<Option<RedialResponse>, FacHostError> {
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

    fn response(_status: &str, dial_count: u32) -> Result<Option<RedialResponse>, FacHostError> {
        Ok(Some(RedialResponse {
            dial_count: Some(dial_count),
        }))
    }

    #[tokio::test]
    async fn first_round_first_poll_confirms_change() {
        let mock = MockTransport::new(vec![ip("203.0.113.11")], vec![response("success", 2)]);
        let result = reconnect_with_transport(
            &mock,
            &config(),
            "203.0.113.10".parse().unwrap(),
            &policy(3),
        )
        .await;
        assert!(result.changed);
        assert_eq!(result.dial_attempts, 1);
        assert_eq!(result.poll_attempts, 1);
        assert_eq!(result.daily_dial_count, Some(2));
    }

    #[tokio::test]
    async fn first_round_later_poll_confirms_change() {
        let mock = MockTransport::new(
            vec![ip("203.0.113.10"), ip("203.0.113.10"), ip("203.0.113.11")],
            vec![response("success", 1)],
        );
        let result = reconnect_with_transport(
            &mock,
            &config(),
            "203.0.113.10".parse().unwrap(),
            &policy(3),
        )
        .await;
        assert!(result.changed);
        assert_eq!(result.poll_attempts, 3);
        assert_eq!(mock.calls(), 1);
    }

    #[tokio::test]
    async fn disconnect_timeout_or_failed_response_still_enters_confirmation() {
        for redial in [
            Err(FacHostError::SshUnavailable),
            Err(FacHostError::Timeout),
            Ok(None),
            response("failed", 2),
        ] {
            let mock = MockTransport::new(vec![ip("203.0.113.11")], vec![redial]);
            let result = reconnect_with_transport(
                &mock,
                &config(),
                "203.0.113.10".parse().unwrap(),
                &policy(1),
            )
            .await;
            assert!(result.changed);
            assert_eq!(mock.calls(), 1);
        }
    }

    #[tokio::test]
    async fn second_round_starts_only_after_complete_first_window() {
        let mock = MockTransport::new(
            vec![ip("203.0.113.10"), ip("203.0.113.10"), ip("203.0.113.11")],
            vec![response("success", 1), response("success", 2)],
        );
        let result = reconnect_with_transport(
            &mock,
            &config(),
            "203.0.113.10".parse().unwrap(),
            &policy(2),
        )
        .await;
        assert!(result.changed);
        assert_eq!(result.dial_attempts, 2);
        assert_eq!(result.poll_attempts, 3);
    }

    #[tokio::test]
    async fn temporary_get_ip_failures_do_not_advance_round_early() {
        let mock = MockTransport::new(
            vec![
                Err(FacHostError::SshUnavailable),
                Err(FacHostError::Timeout),
                ip("203.0.113.11"),
            ],
            vec![response("success", 1)],
        );
        let result = reconnect_with_transport(
            &mock,
            &config(),
            "203.0.113.10".parse().unwrap(),
            &policy(3),
        )
        .await;
        assert!(result.changed);
        assert_eq!(mock.calls(), 1);
    }

    #[tokio::test]
    async fn two_failed_rounds_never_trigger_a_third_redial() {
        let mock = MockTransport::new(
            vec![
                ip("203.0.113.10"),
                ip("203.0.113.10"),
                ip("203.0.113.10"),
                ip("203.0.113.10"),
            ],
            vec![response("success", 1), response("success", 2)],
        );
        let result = reconnect_with_transport(
            &mock,
            &config(),
            "203.0.113.10".parse().unwrap(),
            &policy(2),
        )
        .await;
        assert!(!result.changed);
        assert_eq!(result.dial_attempts, 2);
        assert_eq!(mock.calls(), 2);
    }

    #[test]
    fn rejects_private_or_non_ipv4_addresses() {
        assert!(!is_public_ipv4("192.168.11.213".parse().unwrap()));
        assert!(!is_public_ipv4("100.64.1.1".parse().unwrap()));
        assert!(is_public_ipv4("112.104.136.174".parse().unwrap()));
    }
}
