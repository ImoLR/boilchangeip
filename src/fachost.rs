//! FACHost Seednet dynamic-IP VPS.
//!
//! The router API (`/re-pppoe`) is only reachable from inside the VPS, so every call goes through
//! SSH. The redial usually cuts the SSH session and the router often answers `changed:false` or
//! times out even when the IP really changes, so the outcome is decided by polling the VPS egress
//! IP afterwards.
//!
//! After a redial the SSH host name keeps pointing at the old IP until DDNS and resolver caches
//! catch up, and the old IP is handed to another customer within minutes. When a pinned
//! known_hosts file is configured, SSH uses strict host key checking and a mismatch only means
//! "not ready yet".
//!
//! Round 1 (redial + 5-minute confirmation) runs inline. Anything that needs more time — a second
//! redial when our box still had the old IP, or a late check when it was unreachable — is returned
//! as a follow-up that the caller runs in the background, so the global change lock and the timer's
//! server loop are never held during the wait.

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
/// Hard cap on redials per run.
const MAX_ROUNDS: usize = 2;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FacHostReconnectPolicy {
    pub poll_interval: Duration,
    pub confirmation_window: Duration,
    /// Test cap; production is deadline-driven.
    pub max_poll_attempts_per_round: usize,
    /// Hard-capped at 2.
    pub max_rounds: usize,
    /// Background wait before the follow-up (late check or second redial).
    pub follow_up_delay: Duration,
    /// get_ip attempts when the follow-up checks the current IP.
    pub follow_up_check_attempts: usize,
    pub follow_up_check_interval: Duration,
}

impl Default for FacHostReconnectPolicy {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(10),
            confirmation_window: Duration::from_secs(300),
            max_poll_attempts_per_round: usize::MAX,
            max_rounds: MAX_ROUNDS,
            follow_up_delay: Duration::from_secs(25 * 60),
            follow_up_check_attempts: 3,
            follow_up_check_interval: Duration::from_secs(10),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FacHostFollowUpKind {
    /// Our box answered with the old IP at the end of the window: check again later and redial
    /// once more if it still has the old IP.
    Redial,
    /// Outcome unknown (box unreachable, or the router said success but it was never seen):
    /// only check again later, never redial.
    LateCheck,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FacHostReconnectOutcome {
    pub old_ip: IpAddr,
    pub new_ip: Option<IpAddr>,
    pub changed: bool,
    pub provider_confirmed_success: bool,
    /// At least one redial was sent (or may have been sent).
    pub change_requested: bool,
    /// Our box was reachable at the last check and still had the old IP.
    pub ip_unchanged: bool,
    pub dial_attempts: usize,
    pub poll_attempts: usize,
    pub provider_dial_count: Option<u32>,
    /// Pending background follow-up; the immediate result is "unconfirmed" until it ran.
    pub follow_up: Option<FacHostFollowUpKind>,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FacHostError {
    SshUnavailable,
    HostKeyMismatch,
    Timeout,
    InvalidPublicIpv4,
    InvalidResponse,
}

impl std::fmt::Display for FacHostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::SshUnavailable => "无法通过 SSH 查询 Seednet VPS",
            Self::HostKeyMismatch => {
                "Seednet VPS SSH 主机密钥不匹配（域名可能还指向旧 IP），已拒绝连接"
            }
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

    /// Round 1: redial + confirmation window. May return a pending `follow_up`.
    #[allow(clippy::too_many_arguments)]
    pub async fn reconnect<F, Fut>(
        &self,
        config: &FacHostSeednetConfig,
        server_id: &str,
        server_name: &str,
        old_ip: IpAddr,
        policy: &FacHostReconnectPolicy,
        change_lock: &tokio::sync::Mutex<()>,
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
            change_lock,
            on_provider_confirmed,
        )
        .await
    }

    /// Runs a pending follow-up (waits `follow_up_delay` first). `still_current` is checked after
    /// every wait; when it returns false the follow-up stops without touching the VPS.
    #[allow(clippy::too_many_arguments)]
    pub async fn follow_up<S>(
        &self,
        config: &FacHostSeednetConfig,
        server_id: &str,
        server_name: &str,
        previous: FacHostReconnectOutcome,
        policy: &FacHostReconnectPolicy,
        change_lock: &tokio::sync::Mutex<()>,
        server_lock: &tokio::sync::Mutex<()>,
        still_current: S,
    ) -> Option<FacHostReconnectOutcome>
    where
        S: Fn() -> bool,
    {
        follow_up_with_transport(
            &SshTransport,
            config,
            server_id,
            server_name,
            previous,
            policy,
            change_lock,
            server_lock,
            still_current,
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

/// How a round (redial + confirmation window) ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RoundEnd {
    Changed,
    /// Last poll reached our box and it still had the old IP.
    SameIp,
    /// Last poll failed (unreachable / stale DNS / host key mismatch / timeout).
    Unknown,
    /// SSH never reached our box (host key mismatch), so the redial did not run.
    NotStarted,
}

/// Result of a single "what is the IP now" check.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CheckResult {
    Changed(IpAddr),
    SameIp,
    Unknown,
}

fn minutes(duration: Duration) -> u64 {
    duration.as_secs().div_ceil(60).max(1)
}

fn new_outcome(old_ip: IpAddr) -> FacHostReconnectOutcome {
    FacHostReconnectOutcome {
        old_ip,
        new_ip: None,
        changed: false,
        provider_confirmed_success: false,
        change_requested: false,
        ip_unchanged: false,
        dial_attempts: 0,
        poll_attempts: 0,
        provider_dial_count: None,
        follow_up: None,
        message: "换 IP 后无法确认新的公网 IPv4".to_string(),
    }
}

#[allow(clippy::too_many_arguments)]
async fn reconnect_with_transport<T, F, Fut>(
    transport: &T,
    config: &FacHostSeednetConfig,
    server_id: &str,
    server_name: &str,
    old_ip: IpAddr,
    policy: &FacHostReconnectPolicy,
    change_lock: &tokio::sync::Mutex<()>,
    mut on_provider_confirmed: F,
) -> FacHostReconnectOutcome
where
    T: FacHostTransport,
    F: FnMut(IpAddr, IpAddr) -> Fut,
    Fut: Future<Output = ()>,
{
    let mut outcome = new_outcome(old_ip);
    let max_rounds = policy.max_rounds.clamp(1, MAX_ROUNDS);
    let window_minutes = minutes(policy.confirmation_window);
    let delay_minutes = minutes(policy.follow_up_delay);
    match run_round(
        transport,
        config,
        server_id,
        server_name,
        &mut outcome,
        policy,
        change_lock,
        &mut on_provider_confirmed,
    )
    .await
    {
        RoundEnd::Changed => {}
        RoundEnd::NotStarted => {
            outcome.message = format!("未能发起换 IP：{}", FacHostError::HostKeyMismatch);
        }
        RoundEnd::SameIp if !outcome.provider_confirmed_success && max_rounds > 1 => {
            outcome.ip_unchanged = true;
            outcome.follow_up = Some(FacHostFollowUpKind::Redial);
            outcome.message = format!(
                "第 1 次换 IP 后 {window_minutes} 分钟公网 IP 仍未变化，{delay_minutes} 分钟后自动再换一次，结果另行通知"
            );
        }
        RoundEnd::SameIp if !outcome.provider_confirmed_success => {
            outcome.ip_unchanged = true;
            outcome.message = "换 IP 后公网 IP 仍未变化，已停止自动重试".to_string();
        }
        RoundEnd::SameIp | RoundEnd::Unknown => {
            outcome.follow_up = Some(FacHostFollowUpKind::LateCheck);
            outcome.message = if outcome.provider_confirmed_success {
                format!(
                    "FACHost 已返回换 IP 成功，但 {window_minutes} 分钟内未能确认网络恢复；{delay_minutes} 分钟后再查一次，结果另行通知"
                )
            } else {
                format!(
                    "已发起换 IP，但 {window_minutes} 分钟内无法通过 SSH 确认新 IP（DDNS / 解析缓存可能未更新）；{delay_minutes} 分钟后再查一次，结果另行通知"
                )
            };
        }
    }
    if outcome.follow_up.is_some() {
        log::warn!(
            "FACHost round 1 unconfirmed, follow-up scheduled: server_id={server_id} server_name={server_name:?} follow_up={:?} delay_seconds={} provider_confirmed_success={} polls={}",
            outcome.follow_up,
            policy.follow_up_delay.as_secs(),
            outcome.provider_confirmed_success,
            outcome.poll_attempts,
        );
    }
    outcome
}

#[allow(clippy::too_many_arguments)]
async fn follow_up_with_transport<T, S>(
    transport: &T,
    config: &FacHostSeednetConfig,
    server_id: &str,
    server_name: &str,
    mut outcome: FacHostReconnectOutcome,
    policy: &FacHostReconnectPolicy,
    change_lock: &tokio::sync::Mutex<()>,
    server_lock: &tokio::sync::Mutex<()>,
    still_current: S,
) -> Option<FacHostReconnectOutcome>
where
    T: FacHostTransport,
    S: Fn() -> bool,
{
    let mut kind = outcome.follow_up.take()?;
    let max_rounds = policy.max_rounds.clamp(1, MAX_ROUNDS);
    let delay_minutes = minutes(policy.follow_up_delay);
    loop {
        log::info!(
            "FACHost follow-up waiting: server_id={server_id} server_name={server_name:?} kind={kind:?} delay_seconds={}",
            policy.follow_up_delay.as_secs()
        );
        // Neither lock is held while waiting.
        tokio::time::sleep(policy.follow_up_delay).await;
        if !still_current() {
            log::info!(
                "FACHost follow-up superseded, skipped: server_id={server_id} server_name={server_name:?}"
            );
            return None;
        }
        let _server_guard = server_lock.lock().await;
        let check = check_current_ip(transport, config, server_id, outcome.old_ip, policy).await;
        log::warn!(
            "FACHost follow-up check: server_id={server_id} server_name={server_name:?} kind={kind:?} result={check:?}"
        );
        match check {
            CheckResult::Changed(current_ip) => {
                outcome.new_ip = Some(current_ip);
                outcome.changed = true;
                outcome.ip_unchanged = false;
                outcome.message = "换 IP 已完成（延迟确认）".to_string();
                log::info!(
                    "FACHost reconnect completed late: server_id={server_id} server_name={server_name:?} old_ip={} new_ip={current_ip} dial_attempts={}",
                    outcome.old_ip,
                    outcome.dial_attempts,
                );
                return Some(outcome);
            }
            CheckResult::Unknown => {
                outcome.ip_unchanged = false;
                outcome.message = format!(
                    "已发起换 IP，但 {delay_minutes} 分钟后仍无法通过 SSH 确认新 IP，请稍后查看状态"
                );
                log::warn!(
                    "FACHost follow-up still unknown: server_id={server_id} server_name={server_name:?} dial_attempts={}",
                    outcome.dial_attempts
                );
                return Some(outcome);
            }
            CheckResult::SameIp
                if kind == FacHostFollowUpKind::Redial && outcome.dial_attempts < max_rounds =>
            {
                outcome.ip_unchanged = true;
                let mut no_progress = |_: IpAddr, _: IpAddr| async {};
                match run_round(
                    transport,
                    config,
                    server_id,
                    server_name,
                    &mut outcome,
                    policy,
                    change_lock,
                    &mut no_progress,
                )
                .await
                {
                    RoundEnd::Changed => return Some(outcome),
                    RoundEnd::NotStarted => {
                        outcome.ip_unchanged = true;
                        outcome.message = format!(
                            "第 1 次换 IP 后公网 IP 未变化，第 2 次未能发起：{}",
                            FacHostError::HostKeyMismatch
                        );
                        return Some(outcome);
                    }
                    RoundEnd::SameIp => {
                        outcome.ip_unchanged = true;
                        outcome.message = format!(
                            "已换 IP {} 次，公网 IP 仍未变化，已停止自动重试",
                            outcome.dial_attempts
                        );
                        log::warn!(
                            "FACHost reconnect failed after hard round limit: server_id={server_id} server_name={server_name:?} dial_attempts={} poll_attempts={}",
                            outcome.dial_attempts,
                            outcome.poll_attempts,
                        );
                        return Some(outcome);
                    }
                    RoundEnd::Unknown => {
                        // Round 2 outcome unknown: one more late check, never a third redial.
                        outcome.ip_unchanged = false;
                        kind = FacHostFollowUpKind::LateCheck;
                        continue;
                    }
                }
            }
            CheckResult::SameIp => {
                outcome.ip_unchanged = true;
                outcome.message = if outcome.dial_attempts >= max_rounds {
                    format!(
                        "已换 IP {} 次，公网 IP 仍未变化，已停止自动重试",
                        outcome.dial_attempts
                    )
                } else {
                    format!(
                        "换 IP 后 {delay_minutes} 分钟公网 IP 仍未变化（确认窗口内 VPS 不可达，按设定不再重拨）"
                    )
                };
                log::warn!(
                    "FACHost follow-up confirmed unchanged: server_id={server_id} server_name={server_name:?} dial_attempts={}",
                    outcome.dial_attempts
                );
                return Some(outcome);
            }
        }
    }
}

/// A few get_ip attempts; the first definite answer wins.
async fn check_current_ip<T: FacHostTransport>(
    transport: &T,
    config: &FacHostSeednetConfig,
    server_id: &str,
    old_ip: IpAddr,
    policy: &FacHostReconnectPolicy,
) -> CheckResult {
    for attempt in 1..=policy.follow_up_check_attempts.max(1) {
        if attempt > 1 {
            tokio::time::sleep(policy.follow_up_check_interval).await;
        }
        match timeout(SSH_TIMEOUT, transport.get_ip(config)).await {
            Ok(Ok(ip)) if ip != old_ip => return CheckResult::Changed(ip),
            Ok(Ok(_)) => return CheckResult::SameIp,
            Ok(Err(error)) => log::info!(
                "FACHost follow-up check not ready: server_id={server_id} attempt={attempt} error={error}"
            ),
            Err(_) => log::info!(
                "FACHost follow-up check timed out: server_id={server_id} attempt={attempt}"
            ),
        }
    }
    CheckResult::Unknown
}

/// One redial (CHANGE_IP_LOCK held only while it is sent) + confirmation window.
#[allow(clippy::too_many_arguments)]
async fn run_round<T, F, Fut>(
    transport: &T,
    config: &FacHostSeednetConfig,
    server_id: &str,
    server_name: &str,
    outcome: &mut FacHostReconnectOutcome,
    policy: &FacHostReconnectPolicy,
    change_lock: &tokio::sync::Mutex<()>,
    on_provider_confirmed: &mut F,
) -> RoundEnd
where
    T: FacHostTransport,
    F: FnMut(IpAddr, IpAddr) -> Fut,
    Fut: Future<Output = ()>,
{
    let old_ip = outcome.old_ip;
    let max_rounds = policy.max_rounds.clamp(1, MAX_ROUNDS);
    outcome.dial_attempts += 1;
    let round = outcome.dial_attempts;
    log::info!(
        "FACHost redial started: server_id={server_id} server_name={server_name:?} round={round}/{max_rounds}"
    );

    let redial = {
        let _change_guard = change_lock.lock().await;
        transport.redial(config).await
    };
    let provider_confirmed_ip = match redial {
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
        Err(FacHostError::HostKeyMismatch) => {
            // SSH never reached our box, so /re-pppoe did not run.
            log::warn!(
                "FACHost redial not started: server_id={server_id} server_name={server_name:?} round={round} error={}",
                FacHostError::HostKeyMismatch
            );
            return RoundEnd::NotStarted;
        }
        Err(error) => {
            let result_class = match error {
                FacHostError::Timeout => "timeout",
                FacHostError::SshUnavailable | FacHostError::HostKeyMismatch => {
                    "disconnect_or_transport_error"
                }
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
    outcome.change_requested = true;
    outcome.ip_unchanged = false;

    if let Some(confirmed_ip) = provider_confirmed_ip {
        outcome.provider_confirmed_success = true;
        outcome.new_ip = Some(confirmed_ip);
        outcome.message =
            "FACHost 已明确返回换 IP 成功，但暂时无法确认目标 VPS 网络恢复".to_string();
        on_provider_confirmed(old_ip, confirmed_ip).await;
    }

    // Confirmation window: the lock is not held, other servers proceed.
    let confirmation_started = tokio::time::Instant::now();
    let deadline = confirmation_started + policy.confirmation_window;
    let mut round_poll_attempts = 0usize;
    let mut last_same_ip = false;
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
                    outcome.ip_unchanged = false;
                    outcome.message = "换 IP 已完成".to_string();
                    log::info!(
                        "FACHost reconnect completed: server_id={server_id} server_name={server_name:?} changed=true provider_confirmed_success={} dial_attempts={} final_ip={current_ip} elapsed_seconds={elapsed}",
                        outcome.provider_confirmed_success,
                        outcome.dial_attempts,
                    );
                    return RoundEnd::Changed;
                }
                last_same_ip = true;
                if !outcome.provider_confirmed_success {
                    outcome.new_ip = Some(current_ip);
                }
            }
            Ok(Err(error)) => {
                last_same_ip = false;
                log::warn!(
                    "FACHost confirmation poll not ready: server_id={server_id} server_name={server_name:?} round={round} poll={round_poll_attempts} elapsed_seconds={elapsed} error={error}"
                );
            }
            Err(_) => {
                last_same_ip = false;
                log::warn!(
                    "FACHost confirmation poll timed out at deadline: server_id={server_id} server_name={server_name:?} round={round} poll={round_poll_attempts} elapsed_seconds={elapsed}"
                );
                break;
            }
        }
    }

    log::warn!(
        "FACHost confirmation deadline reached: server_id={server_id} server_name={server_name:?} round={round} last_same_ip={last_same_ip} provider_confirmed_success={} dial_attempts={} polls={round_poll_attempts}",
        outcome.provider_confirmed_success,
        outcome.dial_attempts,
    );
    if last_same_ip {
        RoundEnd::SameIp
    } else {
        RoundEnd::Unknown
    }
}

async fn run_ssh(
    config: &FacHostSeednetConfig,
    remote_command: &str,
) -> Result<String, FacHostError> {
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
        .map_err(|_| FacHostError::Timeout)?
        .map_err(|_| FacHostError::SshUnavailable)?;
    if !output.status.success() {
        if output.status.code() == Some(28) {
            return Err(FacHostError::Timeout);
        }
        // stderr is only classified, never logged.
        return Err(classify_ssh_failure(&String::from_utf8_lossy(
            &output.stderr,
        )));
    }
    String::from_utf8(output.stdout).map_err(|_| FacHostError::InvalidResponse)
}

fn ssh_args(config: &FacHostSeednetConfig) -> Vec<String> {
    let mut args = Vec::new();
    let mut options = vec![
        "BatchMode=yes".to_string(),
        "ConnectTimeout=8".to_string(),
        "ConnectionAttempts=1".to_string(),
    ];
    match &config.known_hosts_path {
        Some(known_hosts_path) => {
            // Ignore ~/.ssh/config and /etc/ssh/ssh_config so nothing can relax the pinning.
            args.push("-F".to_string());
            args.push("/dev/null".to_string());
            options.extend([
                "IdentitiesOnly=yes".to_string(),
                "StrictHostKeyChecking=yes".to_string(),
                format!("UserKnownHostsFile={}", known_hosts_path.display()),
                "GlobalKnownHostsFile=/dev/null".to_string(),
                format!("HostKeyAlias={}", config.host_key_alias()),
                "CheckHostIP=no".to_string(),
                "UpdateHostKeys=no".to_string(),
            ]);
        }
        None => options.push("StrictHostKeyChecking=accept-new".to_string()),
    }
    args.extend([
        "-i".to_string(),
        config.ssh_key_path.display().to_string(),
        "-p".to_string(),
        config.ssh_port.to_string(),
        "-l".to_string(),
        config.ssh_user.clone(),
    ]);
    for option in options {
        args.push("-o".to_string());
        args.push(option);
    }
    args.push("--".to_string());
    args.push(config.ssh_host.clone());
    args
}

fn classify_ssh_failure(stderr: &str) -> FacHostError {
    let lower = stderr.to_ascii_lowercase();
    if lower.contains("host key verification failed")
        || lower.contains("remote host identification has changed")
        || lower.contains("host key for")
    {
        FacHostError::HostKeyMismatch
    } else if lower.contains("timed out") {
        FacHostError::Timeout
    } else {
        FacHostError::SshUnavailable
    }
}

pub(crate) fn is_public_ipv4(ip: Ipv4Addr) -> bool {
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
    type GetResult = Result<IpAddr, FacHostError>;

    #[derive(Clone, Default)]
    struct MockTransport {
        ips: Arc<Mutex<VecDeque<GetResult>>>,
        redials: Arc<Mutex<VecDeque<RedialResult>>>,
        redial_count: Arc<Mutex<usize>>,
        get_count: Arc<Mutex<usize>>,
        /// When set, records whether the change lock was held during each call.
        change_lock: Option<Arc<tokio::sync::Mutex<()>>>,
        lock_held_on_redial: Arc<Mutex<Vec<bool>>>,
        lock_held_on_get_ip: Arc<Mutex<Vec<bool>>>,
    }

    impl MockTransport {
        fn new(ips: Vec<GetResult>, redials: Vec<RedialResult>) -> Self {
            Self {
                ips: Arc::new(Mutex::new(ips.into())),
                redials: Arc::new(Mutex::new(redials.into())),
                ..Self::default()
            }
        }

        fn with_lock(mut self, lock: Arc<tokio::sync::Mutex<()>>) -> Self {
            self.change_lock = Some(lock);
            self
        }

        fn calls(&self) -> usize {
            *self.redial_count.lock().unwrap()
        }

        fn gets(&self) -> usize {
            *self.get_count.lock().unwrap()
        }

        fn lock_held(&self) -> Option<bool> {
            self.change_lock
                .as_ref()
                .map(|lock| lock.try_lock().is_err())
        }
    }

    impl FacHostTransport for MockTransport {
        async fn get_ip(&self, _config: &FacHostSeednetConfig) -> GetResult {
            *self.get_count.lock().unwrap() += 1;
            if let Some(held) = self.lock_held() {
                self.lock_held_on_get_ip.lock().unwrap().push(held);
            }
            // Exhausted queue = box unreachable.
            self.ips
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Err(FacHostError::SshUnavailable))
        }

        async fn redial(&self, _config: &FacHostSeednetConfig) -> RedialResult {
            *self.redial_count.lock().unwrap() += 1;
            if let Some(held) = self.lock_held() {
                self.lock_held_on_redial.lock().unwrap().push(held);
            }
            self.redials.lock().unwrap().pop_front().unwrap()
        }
    }

    fn config() -> FacHostSeednetConfig {
        FacHostSeednetConfig {
            ssh_host: "seednet.example.com".to_string(),
            ssh_port: 22,
            ssh_user: "root".to_string(),
            ssh_key_path: PathBuf::from("/tmp/test-key"),
            known_hosts_path: None,
            host_key_alias: None,
        }
    }

    fn policy(attempts: usize) -> FacHostReconnectPolicy {
        FacHostReconnectPolicy {
            poll_interval: Duration::ZERO,
            confirmation_window: Duration::from_secs(5),
            max_poll_attempts_per_round: attempts,
            max_rounds: 2,
            follow_up_delay: Duration::ZERO,
            follow_up_check_attempts: 3,
            follow_up_check_interval: Duration::ZERO,
        }
    }

    const OLD: &str = "8.8.8.8";
    const NEW: &str = "8.8.4.4";

    fn ip(value: &str) -> GetResult {
        Ok(value.parse().unwrap())
    }

    fn unreachable() -> GetResult {
        Err(FacHostError::SshUnavailable)
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

    async fn run_policy(
        mock: &MockTransport,
        policy: &FacHostReconnectPolicy,
        lock: &tokio::sync::Mutex<()>,
    ) -> FacHostReconnectOutcome {
        reconnect_with_transport(
            mock,
            &config(),
            "seednet-1",
            "Seednet VPS",
            OLD.parse().unwrap(),
            policy,
            lock,
            |_, _| async {},
        )
        .await
    }

    async fn run(mock: &MockTransport, attempts: usize) -> FacHostReconnectOutcome {
        run_policy(mock, &policy(attempts), &tokio::sync::Mutex::new(())).await
    }

    async fn follow(
        mock: &MockTransport,
        previous: FacHostReconnectOutcome,
        policy: &FacHostReconnectPolicy,
    ) -> Option<FacHostReconnectOutcome> {
        follow_up_with_transport(
            mock,
            &config(),
            "seednet-1",
            "Seednet VPS",
            previous,
            policy,
            &tokio::sync::Mutex::new(()),
            &tokio::sync::Mutex::new(()),
            || true,
        )
        .await
    }

    #[tokio::test]
    async fn confirmed_success_never_starts_second_round() {
        let mock = MockTransport::new(vec![ip("1.1.1.1")], vec![confirmed(NEW, 2)]);
        let result = run(&mock, 3).await;
        assert!(result.changed);
        assert!(result.provider_confirmed_success);
        assert_eq!(result.new_ip, Some("1.1.1.1".parse().unwrap()));
        assert_eq!(result.dial_attempts, 1);
        assert_eq!(result.provider_dial_count, Some(2));
        assert_eq!(result.follow_up, None);
        assert_eq!(mock.calls(), 1);
    }

    #[tokio::test]
    async fn success_seen_late_in_the_window_is_success_without_follow_up() {
        // Scaled 5-minute window: unreachable (stale DNS) for the first ~70 %, then the new IP,
        // like a real success seen 3–4 minutes after the redial.
        let mut ips: Vec<GetResult> = (0..35).map(|_| unreachable()).collect();
        ips.push(ip(NEW));
        let mock = MockTransport::new(ips, vec![failed(0)]);
        let test_policy = FacHostReconnectPolicy {
            poll_interval: Duration::from_millis(10),
            confirmation_window: Duration::from_secs(30),
            max_poll_attempts_per_round: usize::MAX,
            ..policy(0)
        };
        let result = run_policy(&mock, &test_policy, &tokio::sync::Mutex::new(())).await;
        assert!(result.changed);
        assert_eq!(result.new_ip, Some(NEW.parse().unwrap()));
        assert_eq!(result.poll_attempts, 36);
        assert_eq!(result.dial_attempts, 1);
        assert_eq!(result.follow_up, None);
        assert_eq!(mock.calls(), 1);
    }

    #[tokio::test]
    async fn unreachable_whole_window_schedules_late_check_without_redial() {
        let mock = MockTransport::new(vec![], vec![response("")]);
        let result = run(&mock, 4).await;
        assert!(!result.changed);
        assert!(result.change_requested);
        assert!(!result.ip_unchanged);
        assert_eq!(result.follow_up, Some(FacHostFollowUpKind::LateCheck));
        assert!(result.message.contains("结果另行通知"));
        assert_eq!(mock.calls(), 1);

        // Late check finds the new IP: late success, still only one redial.
        let late = MockTransport::new(vec![ip(NEW)], vec![]);
        let final_result = follow(&late, result.clone(), &policy(4)).await.unwrap();
        assert!(final_result.changed);
        assert_eq!(final_result.new_ip, Some(NEW.parse().unwrap()));
        assert_eq!(final_result.dial_attempts, 1);
        assert_eq!(final_result.follow_up, None);
        assert_eq!(late.calls(), 0);

        // Late check finds the old IP: confirmed unchanged, but no redial.
        let late = MockTransport::new(vec![ip(OLD)], vec![]);
        let final_result = follow(&late, result.clone(), &policy(4)).await.unwrap();
        assert!(!final_result.changed);
        assert!(final_result.ip_unchanged);
        assert_eq!(final_result.dial_attempts, 1);
        assert_eq!(late.calls(), 0);

        // Late check still unreachable (all attempts): unknown, no redial.
        let late = MockTransport::new(vec![], vec![]);
        let final_result = follow(&late, result, &policy(4)).await.unwrap();
        assert!(!final_result.changed);
        assert!(!final_result.ip_unchanged);
        assert!(final_result.change_requested);
        assert_eq!(late.gets(), 3);
        assert_eq!(late.calls(), 0);
    }

    #[tokio::test]
    async fn reachable_but_unchanged_waits_then_second_round_succeeds() {
        let mock = MockTransport::new(vec![ip(OLD), ip(OLD)], vec![failed(0)]);
        let result = run(&mock, 2).await;
        assert!(!result.changed);
        assert!(result.ip_unchanged);
        assert_eq!(result.follow_up, Some(FacHostFollowUpKind::Redial));
        assert_eq!(mock.calls(), 1);

        // Pre-check: still old → round 2 → new IP after a few unreachable polls.
        let later = MockTransport::new(
            vec![ip(OLD), unreachable(), unreachable(), ip(NEW)],
            vec![response("")],
        );
        let final_result = follow(&later, result, &policy(5)).await.unwrap();
        assert!(final_result.changed);
        assert_eq!(final_result.new_ip, Some(NEW.parse().unwrap()));
        assert_eq!(final_result.dial_attempts, 2);
        assert_eq!(later.calls(), 1);
    }

    #[tokio::test]
    async fn reachable_but_unchanged_twice_fails_after_two_redials() {
        let mock = MockTransport::new(vec![ip(OLD)], vec![failed(0)]);
        let result = run(&mock, 1).await;
        assert_eq!(result.follow_up, Some(FacHostFollowUpKind::Redial));

        let later = MockTransport::new(vec![ip(OLD), ip(OLD), ip(OLD)], vec![failed(0)]);
        let final_result = follow(&later, result, &policy(2)).await.unwrap();
        assert!(!final_result.changed);
        assert!(final_result.ip_unchanged);
        assert_eq!(final_result.dial_attempts, 2);
        assert_eq!(final_result.follow_up, None);
        assert!(final_result.message.contains("已停止自动重试"));
        assert_eq!(later.calls(), 1);
    }

    #[tokio::test]
    async fn late_success_before_second_round_skips_the_redial() {
        let mock = MockTransport::new(vec![ip(OLD)], vec![failed(0)]);
        let result = run(&mock, 1).await;
        let later = MockTransport::new(vec![ip(NEW)], vec![]);
        let final_result = follow(&later, result, &policy(2)).await.unwrap();
        assert!(final_result.changed);
        assert_eq!(final_result.dial_attempts, 1);
        assert_eq!(later.calls(), 0);
    }

    #[tokio::test]
    async fn unreachable_before_second_round_never_redials() {
        let mock = MockTransport::new(vec![ip(OLD)], vec![failed(0)]);
        let result = run(&mock, 1).await;
        let later = MockTransport::new(vec![], vec![]);
        let final_result = follow(&later, result, &policy(2)).await.unwrap();
        assert!(!final_result.changed);
        assert!(!final_result.ip_unchanged);
        assert_eq!(final_result.dial_attempts, 1);
        assert_eq!(later.calls(), 0);
    }

    #[tokio::test]
    async fn second_round_unknown_gets_one_more_late_check_but_no_third_redial() {
        let mock = MockTransport::new(vec![ip(OLD)], vec![failed(0)]);
        let result = run(&mock, 1).await;
        // pre-check old, round 2 polls unreachable (2), late check old.
        let later = MockTransport::new(
            vec![ip(OLD), unreachable(), unreachable(), ip(OLD)],
            vec![response("")],
        );
        let final_result = follow(&later, result, &policy(2)).await.unwrap();
        assert!(!final_result.changed);
        assert!(final_result.ip_unchanged);
        assert_eq!(final_result.dial_attempts, 2);
        assert_eq!(later.calls(), 1);
        assert_eq!(later.gets(), 4);
    }

    #[tokio::test]
    async fn superseded_follow_up_does_nothing() {
        let mock = MockTransport::new(vec![ip(OLD)], vec![failed(0)]);
        let result = run(&mock, 1).await;
        let later = MockTransport::new(vec![ip(OLD)], vec![response("")]);
        let final_result = follow_up_with_transport(
            &later,
            &config(),
            "seednet-1",
            "Seednet VPS",
            result,
            &policy(1),
            &tokio::sync::Mutex::new(()),
            &tokio::sync::Mutex::new(()),
            || false,
        )
        .await;
        assert_eq!(final_result, None);
        assert_eq!(later.gets(), 0);
        assert_eq!(later.calls(), 0);
    }

    #[tokio::test]
    async fn change_lock_is_held_only_while_redialing() {
        let lock = Arc::new(tokio::sync::Mutex::new(()));
        let mock = MockTransport::new(vec![ip(OLD), ip(OLD)], vec![failed(0)])
            .with_lock(Arc::clone(&lock));
        let result = run_policy(&mock, &policy(2), &lock).await;
        assert_eq!(*mock.lock_held_on_redial.lock().unwrap(), vec![true]);
        assert_eq!(
            *mock.lock_held_on_get_ip.lock().unwrap(),
            vec![false, false]
        );

        // Follow-up: during the wait neither lock is held; round 2 takes the change lock only
        // for the redial.
        let server_lock = Arc::new(tokio::sync::Mutex::new(()));
        let later = MockTransport::new(vec![ip(OLD), ip(NEW)], vec![failed(0)])
            .with_lock(Arc::clone(&lock));
        let test_policy = FacHostReconnectPolicy {
            follow_up_delay: Duration::from_millis(300),
            ..policy(2)
        };
        let task = {
            let later = later.clone();
            let lock = Arc::clone(&lock);
            let server_lock = Arc::clone(&server_lock);
            tokio::spawn(async move {
                follow_up_with_transport(
                    &later,
                    &config(),
                    "seednet-1",
                    "Seednet VPS",
                    result,
                    &test_policy,
                    &lock,
                    &server_lock,
                    || true,
                )
                .await
            })
        };
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(lock.try_lock().is_ok(), "change lock held during wait");
        assert!(
            server_lock.try_lock().is_ok(),
            "server lock held during wait"
        );
        let final_result = task.await.unwrap().unwrap();
        assert!(final_result.changed);
        assert_eq!(*later.lock_held_on_redial.lock().unwrap(), vec![true]);
        assert!(later
            .lock_held_on_get_ip
            .lock()
            .unwrap()
            .iter()
            .all(|held| !held));
    }

    #[tokio::test]
    async fn round_one_returns_without_waiting_for_the_follow_up_delay() {
        let mock = MockTransport::new(vec![ip(OLD)], vec![failed(0)]);
        let test_policy = FacHostReconnectPolicy {
            follow_up_delay: Duration::from_secs(3600),
            ..policy(1)
        };
        let started = std::time::Instant::now();
        let result = run_policy(&mock, &test_policy, &tokio::sync::Mutex::new(())).await;
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(result.follow_up, Some(FacHostFollowUpKind::Redial));
        assert!(result.message.contains("60 分钟后自动再换一次"));
    }

    #[tokio::test]
    async fn host_key_mismatch_on_redial_means_not_started() {
        let mock = MockTransport::new(vec![], vec![Err(FacHostError::HostKeyMismatch)]);
        let result = run(&mock, 3).await;
        assert!(!result.changed);
        assert!(!result.change_requested);
        assert_eq!(result.follow_up, None);
        assert_eq!(mock.gets(), 0);
        assert_eq!(mock.calls(), 1);
    }

    #[tokio::test]
    async fn host_key_mismatch_polls_are_not_ready() {
        let mock = MockTransport::new(
            vec![
                Err(FacHostError::HostKeyMismatch),
                Err(FacHostError::HostKeyMismatch),
                ip(NEW),
            ],
            vec![response("")],
        );
        let result = run(&mock, 3).await;
        assert!(result.changed);
        assert_eq!(mock.calls(), 1);
    }

    #[tokio::test]
    async fn confirmed_success_without_network_recovery_only_schedules_late_check() {
        let mock = MockTransport::new(
            vec![unreachable(), Err(FacHostError::Timeout), ip(OLD)],
            vec![confirmed(NEW, 1)],
        );
        let result = run(&mock, 3).await;
        assert!(!result.changed);
        assert!(result.provider_confirmed_success);
        assert_eq!(result.new_ip, Some(NEW.parse().unwrap()));
        assert_eq!(result.follow_up, Some(FacHostFollowUpKind::LateCheck));
        assert_eq!(mock.calls(), 1);
    }

    #[tokio::test]
    async fn timeout_or_disconnect_then_ip_change_succeeds_in_first_round() {
        for redial in [
            Err(FacHostError::Timeout),
            Err(FacHostError::SshUnavailable),
        ] {
            let mock = MockTransport::new(vec![ip(NEW)], vec![redial]);
            let result = run(&mock, 1).await;
            assert!(result.changed);
            assert!(!result.provider_confirmed_success);
            assert_eq!(mock.calls(), 1);
        }
    }

    #[tokio::test]
    async fn malformed_json_then_ip_change_succeeds_in_first_round() {
        let mock = MockTransport::new(vec![ip(NEW)], vec![response("{broken")]);
        let result = run(&mock, 1).await;
        assert!(result.changed);
        assert_eq!(mock.calls(), 1);
    }

    #[tokio::test]
    async fn explicit_failure_still_polls_and_accepts_ip_change() {
        let mock = MockTransport::new(vec![ip(OLD), ip(NEW)], vec![failed(1)]);
        let result = run(&mock, 2).await;
        assert!(result.changed);
        assert_eq!(result.dial_attempts, 1);
        assert_eq!(mock.calls(), 1);
    }

    #[tokio::test]
    async fn hard_round_limit_is_two_even_if_policy_is_higher() {
        let mut test_policy = policy(1);
        test_policy.max_rounds = 99;
        let mock = MockTransport::new(vec![ip(OLD)], vec![response("")]);
        let result = run_policy(&mock, &test_policy, &tokio::sync::Mutex::new(())).await;
        let later = MockTransport::new(
            vec![ip(OLD), ip(OLD), ip(OLD), ip(OLD)],
            vec![response(""), response("")],
        );
        let final_result = follow(&later, result, &test_policy).await.unwrap();
        assert_eq!(final_result.dial_attempts, 2);
        assert_eq!(later.calls(), 1);
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
    fn default_policy_uses_five_minute_window_and_25_minute_follow_up() {
        let policy = FacHostReconnectPolicy::default();
        assert_eq!(policy.poll_interval, Duration::from_secs(10));
        assert_eq!(policy.confirmation_window, Duration::from_secs(300));
        assert_eq!(policy.max_poll_attempts_per_round, usize::MAX);
        assert_eq!(policy.max_rounds, 2);
        assert_eq!(policy.follow_up_delay, Duration::from_secs(25 * 60));
    }

    #[test]
    fn ssh_args_pin_host_key_when_known_hosts_is_configured() {
        let legacy = ssh_args(&config()).join(" ");
        assert!(legacy.contains("StrictHostKeyChecking=accept-new"));
        assert!(!legacy.contains("UserKnownHostsFile"));

        let pinned = FacHostSeednetConfig {
            ssh_host: "facseednet.example.com".to_string(),
            known_hosts_path: Some(PathBuf::from("/etc/boil/keys/seednet-vds-1.known_hosts")),
            ..config()
        };
        let args = ssh_args(&pinned);
        let joined = args.join(" ");
        assert_eq!(&args[..2], ["-F", "/dev/null"]);
        assert!(joined.contains("StrictHostKeyChecking=yes"));
        assert!(!joined.contains("accept-new"));
        assert!(joined.contains("UserKnownHostsFile=/etc/boil/keys/seednet-vds-1.known_hosts"));
        assert!(joined.contains("GlobalKnownHostsFile=/dev/null"));
        assert!(joined.contains("HostKeyAlias=facseednet.example.com"));
        assert!(joined.contains("UpdateHostKeys=no"));
        assert_eq!(&args[args.len() - 2..], ["--", "facseednet.example.com"]);
    }

    #[test]
    fn classifies_host_key_failures() {
        assert_eq!(
            classify_ssh_failure("Host key verification failed."),
            FacHostError::HostKeyMismatch
        );
        assert_eq!(
            classify_ssh_failure("@ WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED! @"),
            FacHostError::HostKeyMismatch
        );
        assert_eq!(
            classify_ssh_failure("ssh: connect to host x port 22: Connection timed out"),
            FacHostError::Timeout
        );
        assert_eq!(
            classify_ssh_failure("Connection refused"),
            FacHostError::SshUnavailable
        );
    }

    #[test]
    fn strict_success_requires_all_provider_evidence() {
        let old_ip = OLD.parse().unwrap();
        let valid = RedialResponse {
            changed: Some(true),
            status: Some("success".to_string()),
            ip: Some(NEW.to_string()),
            old_ip: Some(OLD.to_string()),
            dial_count: Some(1),
        };
        assert_eq!(
            classify_redial_response(Some(200), &valid, old_ip),
            RedialDisposition::Confirmed(NEW.parse().unwrap())
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
