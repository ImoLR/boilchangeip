use std::{
    collections::HashMap,
    future::Future,
    net::IpAddr,
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use crate::{
    boil::{BoilApiError, BoilClient},
    config::{AppConfig, ChangeIpCooldownConfig, ResolvedSelection, ServerConfig, ServerSelection},
};

static SERVER_LOCKS: OnceLock<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> =
    OnceLock::new();
static CHANGE_IP_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
static CHANGE_IP_COOLDOWN: OnceLock<Mutex<ChangeIpCooldownState>> = OnceLock::new();

const SUCCESS_MESSAGE: &str = "换 IP 已完成";
const UNCONFIRMED_MESSAGE: &str =
    "换 IP 请求已被接受，Boil 后端仍在切换，请稍后使用 boil status 或 Telegram /status 查看。";
const CHANGE_RESPONSE_UNCONFIRMED_MESSAGE: &str =
    "换 IP 请求已发出，但 Boil API 响应暂时无法确认，正在查询最终 IP。";
const CHANGE_IP_COOLDOWN_SAFETY_MARGIN: Duration = Duration::from_secs(3);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReconnectPolicy {
    pub initial_delay: Duration,
    pub poll_interval: Duration,
    pub max_poll_attempts: usize,
}

impl Default for ReconnectPolicy {
    fn default() -> Self {
        Self {
            initial_delay: Duration::from_secs(2),
            poll_interval: Duration::from_secs(2),
            max_poll_attempts: 3,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReconnectStatus {
    Success,
    Disabled,
    PreflightFailed,
    RateLimited,
    ApiRejected,
    ChangeAcceptedButUnconfirmed,
    InvalidResponse,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReconnectResult {
    pub server_id: String,
    pub server_name: String,
    pub old_ip: Option<IpAddr>,
    pub new_ip: Option<IpAddr>,
    pub changed: bool,
    pub uses_left: Option<u32>,
    pub next_allowed_at: Option<i64>,
    pub status: ReconnectStatus,
    pub message: Option<String>,
    pub poll_attempts: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReconnectProgress {
    VerifyingNewIp { old_ip: IpAddr },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangeIpCooldownMode {
    FailFast,
    Wait,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReconnectCooldown {
    pub remaining: Duration,
    pub available_at: i64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BatchReconnectResult {
    pub results: Vec<ReconnectResult>,
}

impl BatchReconnectResult {
    pub fn success_count(&self) -> usize {
        self.results
            .iter()
            .filter(|result| result.status == ReconnectStatus::Success)
            .count()
    }

    pub fn unconfirmed_count(&self) -> usize {
        self.results
            .iter()
            .filter(|result| result.status == ReconnectStatus::ChangeAcceptedButUnconfirmed)
            .count()
    }

    pub fn failure_count(&self) -> usize {
        self.results.len() - self.success_count() - self.unconfirmed_count()
    }
}

pub async fn reconnect_one(
    client: &BoilClient,
    server: &ServerConfig,
    policy: &ReconnectPolicy,
) -> ReconnectResult {
    reconnect_one_with_current_ip(client, server, policy, None).await
}

pub async fn reconnect_one_with_current_ip(
    client: &BoilClient,
    server: &ServerConfig,
    policy: &ReconnectPolicy,
    current_ip: Option<IpAddr>,
) -> ReconnectResult {
    reconnect_one_with_current_ip_and_progress(client, server, policy, current_ip, |_| async {})
        .await
}

pub async fn reconnect_one_with_current_ip_and_progress<F, Fut>(
    client: &BoilClient,
    server: &ServerConfig,
    policy: &ReconnectPolicy,
    current_ip: Option<IpAddr>,
    on_progress: F,
) -> ReconnectResult
where
    F: FnMut(ReconnectProgress) -> Fut,
    Fut: Future<Output = ()>,
{
    reconnect_one_with_current_ip_progress_and_cooldown(
        client,
        server,
        policy,
        current_ip,
        ChangeIpCooldownMode::FailFast,
        on_progress,
    )
    .await
}

pub async fn reconnect_one_with_current_ip_progress_and_cooldown<F, Fut>(
    client: &BoilClient,
    server: &ServerConfig,
    policy: &ReconnectPolicy,
    current_ip: Option<IpAddr>,
    cooldown_mode: ChangeIpCooldownMode,
    on_progress: F,
) -> ReconnectResult
where
    F: FnMut(ReconnectProgress) -> Fut,
    Fut: Future<Output = ()>,
{
    reconnect_one_with_current_ip_progress_cooldown_notify(
        client,
        server,
        policy,
        current_ip,
        cooldown_mode,
        on_progress,
        |_| async {},
    )
    .await
}

pub async fn reconnect_one_with_current_ip_progress_cooldown_notify<F, Fut, C, CFut>(
    client: &BoilClient,
    server: &ServerConfig,
    policy: &ReconnectPolicy,
    current_ip: Option<IpAddr>,
    cooldown_mode: ChangeIpCooldownMode,
    on_progress: F,
    on_cooldown: C,
) -> ReconnectResult
where
    F: FnMut(ReconnectProgress) -> Fut,
    Fut: Future<Output = ()>,
    C: FnMut(ReconnectCooldown) -> CFut,
    CFut: Future<Output = ()>,
{
    if !server.enabled {
        return base_result(
            server,
            ReconnectStatus::Disabled,
            Some("server is disabled"),
        );
    }

    let lock = server_lock(&server.id);
    let _guard = lock.lock().await;
    reconnect_one_locked(
        client,
        server,
        policy,
        current_ip,
        cooldown_mode,
        on_progress,
        on_cooldown,
    )
    .await
}

pub async fn reconnect_selected(
    client: &BoilClient,
    config: &AppConfig,
    selection: ServerSelection<'_>,
    policy: &ReconnectPolicy,
) -> anyhow::Result<BatchReconnectResult> {
    let selected = config.resolve_servers(selection)?;
    let mut results = Vec::new();

    match selected {
        ResolvedSelection::One(server) => {
            results.push(reconnect_one(client, server, policy).await);
        }
        ResolvedSelection::All(servers) => {
            for server in servers {
                results.push(reconnect_one(client, server, policy).await);
            }
        }
    }

    Ok(BatchReconnectResult { results })
}

async fn reconnect_one_locked<F, Fut, C, CFut>(
    client: &BoilClient,
    server: &ServerConfig,
    policy: &ReconnectPolicy,
    current_ip: Option<IpAddr>,
    cooldown_mode: ChangeIpCooldownMode,
    mut on_progress: F,
    on_cooldown: C,
) -> ReconnectResult
where
    F: FnMut(ReconnectProgress) -> Fut,
    Fut: Future<Output = ()>,
    C: FnMut(ReconnectCooldown) -> CFut,
    CFut: Future<Output = ()>,
{
    let old_ip = match current_ip {
        Some(ip) => ip,
        None => match client.get_ip(&server.token).await {
            Ok(response) => response.ip,
            Err(error) => {
                let status = preflight_error_status(&error);
                return base_result(server, status, Some(&error.to_string()));
            }
        },
    };

    let change = change_ip_with_cooldown(client, server, cooldown_mode, on_cooldown).await;
    let mut result = match change {
        Ok(response) => {
            let mut result = base_result(
                server,
                ReconnectStatus::ChangeAcceptedButUnconfirmed,
                Some(UNCONFIRMED_MESSAGE),
            );
            result.old_ip = Some(old_ip);
            result.uses_left = response.uses_left;
            result.next_allowed_at = response.next_allowed_at.filter(|timestamp| *timestamp >= 0);
            result
        }
        Err(ChangeIpAttemptError::Cooldown(cooldown)) => {
            let mut result = base_result(
                server,
                ReconnectStatus::RateLimited,
                Some(&manual_cooldown_message(cooldown.remaining)),
            );
            result.old_ip = Some(old_ip);
            result.next_allowed_at = Some(cooldown.available_at);
            return result;
        }
        Err(ChangeIpAttemptError::Api(error)) => {
            let status = change_error_status(&error);
            if status == ReconnectStatus::ChangeAcceptedButUnconfirmed {
                let mut result =
                    base_result(server, status, Some(CHANGE_RESPONSE_UNCONFIRMED_MESSAGE));
                result.old_ip = Some(old_ip);
                result
            } else {
                let mut result = base_result(server, status, Some(&error.to_string()));
                result.old_ip = Some(old_ip);
                return result;
            }
        }
    };

    on_progress(ReconnectProgress::VerifyingNewIp { old_ip }).await;

    tokio::time::sleep(policy.initial_delay).await;

    for attempt in 1..=policy.max_poll_attempts {
        result.poll_attempts = attempt;
        match client.get_ip(&server.token).await {
            Ok(response) if response.ip != old_ip => {
                result.new_ip = Some(response.ip);
                result.changed = true;
                result.status = ReconnectStatus::Success;
                result.message = Some(SUCCESS_MESSAGE.to_string());
                return result;
            }
            Ok(_) => {}
            Err(error) => {
                log::debug!(
                    "换 IP 后验证暂时失败: server_id={} attempt={attempt}: {}",
                    result.server_id,
                    redact_for_result(&error.to_string(), server)
                );
            }
        }

        if attempt < policy.max_poll_attempts {
            tokio::time::sleep(policy.poll_interval).await;
        }
    }

    result
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ChangeIpCooldownHit {
    remaining: Duration,
    available_at: i64,
}

impl From<ChangeIpCooldownHit> for ReconnectCooldown {
    fn from(hit: ChangeIpCooldownHit) -> Self {
        Self {
            remaining: hit.remaining,
            available_at: hit.available_at,
        }
    }
}

#[derive(Debug)]
enum ChangeIpAttemptError {
    Api(BoilApiError),
    Cooldown(ChangeIpCooldownHit),
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct ChangeIpCooldownState {
    cooldown: Option<Duration>,
    last_changeip_at: Option<i64>,
    next_changeip_available_at: Option<i64>,
}

impl From<&ChangeIpCooldownConfig> for ChangeIpCooldownState {
    fn from(config: &ChangeIpCooldownConfig) -> Self {
        Self {
            cooldown: config.cooldown_seconds.map(Duration::from_secs),
            last_changeip_at: config.last_changeip_at,
            next_changeip_available_at: config.next_changeip_available_at,
        }
    }
}

impl ChangeIpCooldownState {
    #[cfg(not(test))]
    fn to_config(&self) -> Option<ChangeIpCooldownConfig> {
        (self.cooldown.is_some()
            || self.last_changeip_at.is_some()
            || self.next_changeip_available_at.is_some())
        .then(|| ChangeIpCooldownConfig {
            cooldown_seconds: self.cooldown.map(|duration| duration.as_secs()),
            last_changeip_at: self.last_changeip_at,
            next_changeip_available_at: self.next_changeip_available_at,
        })
    }

    fn cooldown_hit_at(&self, now: i64) -> Option<ChangeIpCooldownHit> {
        let available_at = self.next_changeip_available_at?;
        let remaining_seconds = available_at.saturating_sub(now);
        (remaining_seconds > 0).then(|| ChangeIpCooldownHit {
            remaining: Duration::from_secs(remaining_seconds as u64),
            available_at,
        })
    }

    fn record_success(&mut self, request_at: i64) {
        self.last_changeip_at = Some(request_at);
        if let Some(cooldown) = self.cooldown {
            self.next_changeip_available_at = request_at.checked_add(duration_secs_i64(cooldown));
        }
    }

    fn record_rate_limit(&mut self, request_at: i64, api_next_available_at: i64) {
        let adjusted_available_at = api_next_available_at
            .saturating_add(duration_secs_i64(CHANGE_IP_COOLDOWN_SAFETY_MARGIN));
        let base_at = self.last_changeip_at.unwrap_or(request_at);
        let observed_seconds = adjusted_available_at.saturating_sub(base_at);
        let observed = Duration::from_secs(observed_seconds.max(1) as u64);
        let previous = self.cooldown.unwrap_or_default();
        if observed > previous {
            log::info!(
                "changeIP cooldown 修正: previous={}s observed={}s api_next_available_at={api_next_available_at}",
                previous.as_secs(),
                observed.as_secs()
            );
            self.cooldown = Some(observed);
        }
        self.next_changeip_available_at = Some(adjusted_available_at);
    }
}

async fn change_ip_with_cooldown<F, Fut>(
    client: &BoilClient,
    server: &ServerConfig,
    mode: ChangeIpCooldownMode,
    mut on_cooldown: F,
) -> Result<crate::boil::ChangeIpResponse, ChangeIpAttemptError>
where
    F: FnMut(ReconnectCooldown) -> Fut,
    Fut: Future<Output = ()>,
{
    loop {
        let request_at = unix_now();
        let wait = {
            let _change_guard = CHANGE_IP_LOCK.lock().await;
            let state = change_ip_cooldown_state();
            let state = state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state.cooldown_hit_at(request_at)
        };

        if let Some(cooldown) = wait {
            log::info!(
                "changeIP cooldown active: server_id={} cooldown_wait={}s next_available_at={}",
                server.id,
                cooldown.remaining.as_secs(),
                cooldown.available_at
            );
            match mode {
                ChangeIpCooldownMode::FailFast => {
                    return Err(ChangeIpAttemptError::Cooldown(cooldown));
                }
                ChangeIpCooldownMode::Wait => {
                    on_cooldown(cooldown.into()).await;
                    tokio::time::sleep(cooldown.remaining).await;
                    continue;
                }
            }
        }

        let _change_guard = CHANGE_IP_LOCK.lock().await;
        let request_at = unix_now();
        log::info!(
            "changeIP request: server_id={} cooldown={}s last_changeip_at={:?} next_available_at={:?}",
            server.id,
            current_cooldown_seconds(),
            current_last_changeip_at(),
            current_next_changeip_available_at()
        );
        match client.change_ip(&server.token).await {
            Ok(response) => {
                update_change_ip_cooldown(|state| state.record_success(request_at));
                return Ok(response);
            }
            Err(error) => {
                if let Some(api_next_available_at) = parse_rate_limit_next_available_at(&error) {
                    update_change_ip_cooldown(|state| {
                        state.record_rate_limit(request_at, api_next_available_at)
                    });
                    let cooldown = cooldown_hit_or_zero(unix_now(), api_next_available_at);
                    log::info!(
                        "changeIP rate limited: server_id={} api_next_available_at={} wait={}s",
                        server.id,
                        api_next_available_at,
                        cooldown.remaining.as_secs()
                    );
                    match mode {
                        ChangeIpCooldownMode::FailFast => {
                            return Err(ChangeIpAttemptError::Cooldown(cooldown));
                        }
                        ChangeIpCooldownMode::Wait => {
                            if cooldown.remaining > Duration::ZERO {
                                on_cooldown(cooldown.into()).await;
                                tokio::time::sleep(cooldown.remaining).await;
                            }
                            continue;
                        }
                    }
                }
                return Err(ChangeIpAttemptError::Api(error));
            }
        }
    }
}

fn base_result(
    server: &ServerConfig,
    status: ReconnectStatus,
    message: Option<&str>,
) -> ReconnectResult {
    ReconnectResult {
        server_id: redact_for_result(&server.id, server),
        server_name: redact_for_result(&server.name, server),
        old_ip: None,
        new_ip: None,
        changed: false,
        uses_left: None,
        next_allowed_at: None,
        status,
        message: message.map(|value| redact_for_result(value, server)),
        poll_attempts: 0,
    }
}

fn redact_for_result(value: &str, server: &ServerConfig) -> String {
    let token = server.token.expose_secret();
    if token.is_empty() {
        value.to_string()
    } else {
        value.replace(token, "<redacted>")
    }
}

fn preflight_error_status(error: &BoilApiError) -> ReconnectStatus {
    match error {
        BoilApiError::InvalidJson { .. } | BoilApiError::InvalidResponse(_) => {
            ReconnectStatus::InvalidResponse
        }
        BoilApiError::Transport(_) | BoilApiError::HttpStatus { .. } => {
            ReconnectStatus::PreflightFailed
        }
        BoilApiError::ApiRejected { .. } => ReconnectStatus::ApiRejected,
    }
}

fn change_error_status(error: &BoilApiError) -> ReconnectStatus {
    match error {
        BoilApiError::ApiRejected { .. } => ReconnectStatus::ApiRejected,
        BoilApiError::InvalidJson { .. } | BoilApiError::InvalidResponse(_) => {
            ReconnectStatus::InvalidResponse
        }
        BoilApiError::Transport(_) | BoilApiError::HttpStatus { .. } => {
            ReconnectStatus::ChangeAcceptedButUnconfirmed
        }
    }
}

#[cfg(test)]
async fn with_change_ip_lock<F, R>(future: F) -> R
where
    F: std::future::Future<Output = R>,
{
    let _change_guard = CHANGE_IP_LOCK.lock().await;
    future.await
}

fn change_ip_cooldown_state() -> &'static Mutex<ChangeIpCooldownState> {
    CHANGE_IP_COOLDOWN.get_or_init(|| Mutex::new(load_persisted_change_ip_cooldown()))
}

#[cfg(not(test))]
fn load_persisted_change_ip_cooldown() -> ChangeIpCooldownState {
    crate::config::load_app_config()
        .ok()
        .and_then(|config| config.change_ip_cooldown)
        .as_ref()
        .map(ChangeIpCooldownState::from)
        .unwrap_or_default()
}

#[cfg(test)]
fn load_persisted_change_ip_cooldown() -> ChangeIpCooldownState {
    ChangeIpCooldownState::default()
}

#[cfg(not(test))]
fn update_change_ip_cooldown(update: impl FnOnce(&mut ChangeIpCooldownState)) {
    let snapshot = {
        let state = change_ip_cooldown_state();
        let mut state = state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        update(&mut state);
        state.clone()
    };

    if let Some(config) = snapshot.to_config() {
        if let Err(error) = crate::config::persist_change_ip_cooldown(&config) {
            log::warn!("changeIP cooldown 持久化失败: {error}");
        }
    }
}

#[cfg(test)]
fn update_change_ip_cooldown(_update: impl FnOnce(&mut ChangeIpCooldownState)) {}

fn current_cooldown_seconds() -> u64 {
    let state = change_ip_cooldown_state();
    let state = state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    state
        .cooldown
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn current_last_changeip_at() -> Option<i64> {
    let state = change_ip_cooldown_state();
    let state = state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    state.last_changeip_at
}

fn current_next_changeip_available_at() -> Option<i64> {
    let state = change_ip_cooldown_state();
    let state = state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    state.next_changeip_available_at
}

fn parse_rate_limit_next_available_at(error: &BoilApiError) -> Option<i64> {
    let BoilApiError::ApiRejected { message, .. } = error else {
        return None;
    };
    if !(message.contains("频率限制") && message.contains("下次可用时间")) {
        return None;
    }
    last_integer_in_text(message)
}

fn last_integer_in_text(text: &str) -> Option<i64> {
    text.split(|ch: char| !ch.is_ascii_digit())
        .filter(|part| !part.is_empty())
        .filter_map(|part| part.parse::<i64>().ok())
        .next_back()
}

fn cooldown_hit_or_zero(now: i64, api_next_available_at: i64) -> ChangeIpCooldownHit {
    let available_at =
        api_next_available_at.saturating_add(duration_secs_i64(CHANGE_IP_COOLDOWN_SAFETY_MARGIN));
    let remaining = available_at.saturating_sub(now).max(0) as u64;
    ChangeIpCooldownHit {
        remaining: Duration::from_secs(remaining),
        available_at,
    }
}

fn manual_cooldown_message(remaining: Duration) -> String {
    format!(
        "⏳ 换 IP 频率限制中\n\n预计 {}后可再次换 IP",
        format_wait_duration(remaining)
    )
}

fn format_wait_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    if seconds < 60 {
        format!("{seconds} 秒")
    } else if seconds < 3600 {
        format!("{} 分钟", seconds.div_ceil(60))
    } else {
        let hours = seconds / 3600;
        let minutes = (seconds % 3600).div_ceil(60);
        if minutes == 0 {
            format!("{hours} 小时")
        } else {
            format!("{hours} 小时 {minutes} 分钟")
        }
    }
}

fn duration_secs_i64(duration: Duration) -> i64 {
    i64::try_from(duration.as_secs()).unwrap_or(i64::MAX)
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
        .try_into()
        .unwrap_or(i64::MAX)
}

fn server_lock(server_id: &str) -> Arc<tokio::sync::Mutex<()>> {
    let locks = SERVER_LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut locks = locks
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    Arc::clone(
        locks
            .entry(server_id.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SecretToken;
    use std::{
        collections::VecDeque,
        sync::atomic::{AtomicUsize, Ordering},
    };
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::{TcpListener, TcpStream},
    };

    #[derive(Clone)]
    struct MockResponse {
        status: u16,
        body: &'static str,
        delay: Duration,
        disconnect: bool,
    }

    struct MockServer {
        base_url: String,
        records: Arc<Mutex<Vec<String>>>,
        request_count: Arc<AtomicUsize>,
        max_active_changes: Arc<AtomicUsize>,
    }

    impl MockServer {
        async fn start(responses: Vec<MockResponse>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let responses = Arc::new(Mutex::new(VecDeque::from(responses)));
            let records = Arc::new(Mutex::new(Vec::new()));
            let request_count = Arc::new(AtomicUsize::new(0));
            let active_changes = Arc::new(AtomicUsize::new(0));
            let max_active_changes = Arc::new(AtomicUsize::new(0));

            tokio::spawn({
                let responses = Arc::clone(&responses);
                let records = Arc::clone(&records);
                let request_count = Arc::clone(&request_count);
                let active_changes = Arc::clone(&active_changes);
                let max_active_changes = Arc::clone(&max_active_changes);
                async move {
                    while let Ok((stream, _)) = listener.accept().await {
                        request_count.fetch_add(1, Ordering::SeqCst);
                        let response = responses.lock().unwrap().pop_front();
                        let Some(response) = response else { break };
                        tokio::spawn(handle_connection(
                            stream,
                            response,
                            Arc::clone(&records),
                            Arc::clone(&active_changes),
                            Arc::clone(&max_active_changes),
                        ));
                    }
                }
            });

            Self {
                base_url: format!("http://{addr}"),
                records,
                request_count,
                max_active_changes,
            }
        }

        fn records(&self) -> Vec<String> {
            self.records.lock().unwrap().clone()
        }

        fn change_count(&self) -> usize {
            self.records()
                .iter()
                .filter(|path| path.as_str() == "/api/v1/changeIP/")
                .count()
        }

        fn request_count(&self) -> usize {
            self.request_count.load(Ordering::SeqCst)
        }

        fn max_active_changes(&self) -> usize {
            self.max_active_changes.load(Ordering::SeqCst)
        }
    }

    async fn handle_connection(
        mut stream: TcpStream,
        response: MockResponse,
        records: Arc<Mutex<Vec<String>>>,
        active_changes: Arc<AtomicUsize>,
        max_active_changes: Arc<AtomicUsize>,
    ) {
        let mut buffer = Vec::new();
        let mut chunk = [0u8; 1024];
        loop {
            let read = stream.read(&mut chunk).await.unwrap_or(0);
            if read == 0 {
                return;
            }
            buffer.extend_from_slice(&chunk[..read]);
            if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }

        let request = String::from_utf8_lossy(&buffer);
        let path = request
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .unwrap_or_default()
            .to_string();
        records.lock().unwrap().push(path.clone());

        let is_change = path == "/api/v1/changeIP/";
        if is_change {
            let active = active_changes.fetch_add(1, Ordering::SeqCst) + 1;
            max_active_changes.fetch_max(active, Ordering::SeqCst);
        }
        tokio::time::sleep(response.delay).await;

        if response.disconnect {
            if is_change {
                active_changes.fetch_sub(1, Ordering::SeqCst);
            }
            return;
        }

        let status = match response.status {
            200 => "200 OK",
            400 => "400 Bad Request",
            500 => "500 Internal Server Error",
            _ => "418 Unknown",
        };
        let wire = format!(
            concat!(
                "HTTP/1.1 {}\r\n",
                "content-type: application/json\r\n",
                "content-length: {}\r\n",
                "connection: close\r\n\r\n{}"
            ),
            status,
            response.body.len(),
            response.body
        );
        let _ = stream.write_all(wire.as_bytes()).await;
        if is_change {
            active_changes.fetch_sub(1, Ordering::SeqCst);
        }
    }

    fn response(status: u16, body: &'static str) -> MockResponse {
        MockResponse {
            status,
            body,
            delay: Duration::ZERO,
            disconnect: false,
        }
    }

    fn delayed_response(status: u16, body: &'static str) -> MockResponse {
        MockResponse {
            status,
            body,
            delay: Duration::from_millis(30),
            disconnect: false,
        }
    }

    fn disconnect() -> MockResponse {
        MockResponse {
            status: 200,
            body: "",
            delay: Duration::ZERO,
            disconnect: true,
        }
    }

    fn ip(ip: &'static str) -> MockResponse {
        response(200, ip)
    }

    fn accepted() -> MockResponse {
        response(
            200,
            r#"{"ok":true,"message":"accepted","uses_left":2,"next_allowed_at":1782732942}"#,
        )
    }

    fn server(id: &str, enabled: bool) -> ServerConfig {
        ServerConfig {
            id: id.to_string(),
            name: format!("Server {id}"),
            token: SecretToken::from_test_value(&test_credential()),
            enabled,
            address: None,
            country: None,
            flag: None,
            resolved_ip: None,
            timer: None,
        }
    }

    fn config(servers: Vec<ServerConfig>) -> AppConfig {
        AppConfig {
            servers,
            global_timer: None,
            change_ip_cooldown: None,
            tg_token: None,
            tg_chat_id: None,
            tg_pair_code: None,
            tg_pair_expires_at: None,
            migration_notice: None,
        }
    }

    fn policy(attempts: usize) -> ReconnectPolicy {
        ReconnectPolicy {
            initial_delay: Duration::ZERO,
            poll_interval: Duration::ZERO,
            max_poll_attempts: attempts,
        }
    }

    fn test_credential() -> String {
        ["phase", "three", "credential"].join("-")
    }

    fn rate_limited(next_available_at: i64) -> MockResponse {
        response(
            400,
            Box::leak(
                format!(r#"{{"error":"频率限制中，下次可用时间: {next_available_at}"}}"#)
                    .into_boxed_str(),
            ),
        )
    }

    #[test]
    fn rate_limit_parser_extracts_next_available_timestamp() {
        let error = BoilApiError::ApiRejected {
            status: Some(reqwest::StatusCode::BAD_REQUEST),
            message: "频率限制中，下次可用时间: 1785704991".to_string(),
        };

        assert_eq!(parse_rate_limit_next_available_at(&error), Some(1785704991));
    }

    #[test]
    fn cooldown_state_only_increases_when_api_proves_it_is_too_short() {
        let mut state = ChangeIpCooldownState {
            cooldown: Some(Duration::from_secs(30)),
            last_changeip_at: Some(100),
            next_changeip_available_at: None,
        };

        state.record_rate_limit(130, 120);
        assert_eq!(state.cooldown, Some(Duration::from_secs(30)));

        state.record_rate_limit(130, 160);
        assert_eq!(state.cooldown, Some(Duration::from_secs(63)));
        assert_eq!(state.next_changeip_available_at, Some(163));
    }

    #[test]
    fn successful_change_sets_next_available_when_cooldown_is_known() {
        let mut state = ChangeIpCooldownState {
            cooldown: Some(Duration::from_secs(60)),
            last_changeip_at: None,
            next_changeip_available_at: None,
        };

        state.record_success(1000);

        assert_eq!(state.last_changeip_at, Some(1000));
        assert_eq!(state.next_changeip_available_at, Some(1060));
    }

    #[test]
    fn known_cooldown_can_be_reported_without_requesting_api() {
        let state = ChangeIpCooldownState {
            cooldown: Some(Duration::from_secs(60)),
            last_changeip_at: Some(1000),
            next_changeip_available_at: Some(1060),
        };

        let hit = state.cooldown_hit_at(1001).unwrap();

        assert_eq!(hit.remaining, Duration::from_secs(59));
        assert_eq!(
            manual_cooldown_message(hit.remaining),
            "⏳ 换 IP 频率限制中\n\n预计 59 秒后可再次换 IP"
        );
    }

    #[tokio::test]
    async fn reconnect_succeeds_with_one_change_request() {
        let mock = MockServer::start(vec![
            ip(r#"{"ok":true,"ip":"42.1.1.1"}"#),
            accepted(),
            ip(r#"{"ok":true,"ip":"42.1.1.2"}"#),
        ])
        .await;
        let client = BoilClient::with_api_base_url(&mock.base_url).unwrap();

        let result = reconnect_one(&client, &server("success", true), &policy(3)).await;

        assert_eq!(result.status, ReconnectStatus::Success);
        assert!(result.changed);
        assert_eq!(result.poll_attempts, 1);
        assert_eq!(result.uses_left, Some(2));
        assert_eq!(result.message.as_deref(), Some(SUCCESS_MESSAGE));
        assert_eq!(mock.change_count(), 1);
    }

    #[tokio::test]
    async fn reconnect_polls_without_repeating_change() {
        let mock = MockServer::start(vec![
            ip(r#"{"ok":true,"ip":"42.1.1.1"}"#),
            accepted(),
            ip(r#"{"ok":true,"ip":"42.1.1.1"}"#),
            ip(r#"{"ok":true,"ip":"42.1.1.1"}"#),
            ip(r#"{"ok":true,"ip":"42.1.1.2"}"#),
        ])
        .await;
        let client = BoilClient::with_api_base_url(&mock.base_url).unwrap();

        let result = reconnect_one(&client, &server("poll", true), &policy(3)).await;

        assert_eq!(result.status, ReconnectStatus::Success);
        assert_eq!(result.poll_attempts, 3);
        assert_eq!(mock.change_count(), 1);
    }

    #[tokio::test]
    async fn unchanged_ip_is_unconfirmed_without_second_change() {
        let mock = MockServer::start(vec![
            ip(r#"{"ok":true,"ip":"42.1.1.1"}"#),
            accepted(),
            ip(r#"{"ok":true,"ip":"42.1.1.1"}"#),
            ip(r#"{"ok":true,"ip":"42.1.1.1"}"#),
        ])
        .await;
        let client = BoilClient::with_api_base_url(&mock.base_url).unwrap();

        let result = reconnect_one(&client, &server("unchanged", true), &policy(2)).await;

        assert_eq!(result.status, ReconnectStatus::ChangeAcceptedButUnconfirmed);
        assert_eq!(result.poll_attempts, 2);
        assert_eq!(mock.change_count(), 1);
    }

    #[tokio::test]
    async fn transient_http_400_then_new_ip_succeeds_without_second_change() {
        let mock = MockServer::start(vec![
            ip(r#"{"ok":true,"ip":"42.1.1.1"}"#),
            accepted(),
            response(400, r#"{"error":"temporary backend error"}"#),
            ip(r#"{"ok":true,"ip":"42.1.1.2"}"#),
        ])
        .await;
        let client = BoilClient::with_api_base_url(&mock.base_url).unwrap();

        let result = reconnect_one(&client, &server("http-400", true), &policy(3)).await;

        assert_eq!(result.status, ReconnectStatus::Success);
        assert!(result.changed);
        assert_eq!(result.poll_attempts, 2);
        assert_eq!(mock.change_count(), 1);
    }

    #[tokio::test]
    async fn three_http_400_responses_are_unconfirmed_without_second_change() {
        let mock = MockServer::start(vec![
            ip(r#"{"ok":true,"ip":"42.1.1.1"}"#),
            accepted(),
            response(400, r#"{"error":"temporary backend error"}"#),
            response(400, r#"{"error":"temporary backend error"}"#),
            response(400, r#"{"error":"temporary backend error"}"#),
        ])
        .await;
        let client = BoilClient::with_api_base_url(&mock.base_url).unwrap();

        let result = reconnect_one(&client, &server("http-400", true), &policy(3)).await;

        assert_eq!(result.status, ReconnectStatus::ChangeAcceptedButUnconfirmed);
        assert_eq!(result.poll_attempts, 3);
        assert_eq!(mock.change_count(), 1);
        assert_eq!(mock.request_count(), 5);
        assert_eq!(result.message.as_deref(), Some(UNCONFIRMED_MESSAGE));
    }

    #[tokio::test]
    async fn preflight_failure_never_calls_change() {
        let mock = MockServer::start(vec![response(500, "server unavailable")]).await;
        let client = BoilClient::with_api_base_url(&mock.base_url).unwrap();

        let result = reconnect_one(&client, &server("preflight", true), &policy(1)).await;

        assert_eq!(result.status, ReconnectStatus::PreflightFailed);
        assert_eq!(mock.change_count(), 0);
    }

    #[tokio::test]
    async fn rejected_change_is_not_retried_or_polled() {
        let mock = MockServer::start(vec![
            ip(r#"{"ok":true,"ip":"42.1.1.1"}"#),
            response(400, r#"{"error":"quota exhausted"}"#),
        ])
        .await;
        let client = BoilClient::with_api_base_url(&mock.base_url).unwrap();

        let result = reconnect_one(&client, &server("rejected", true), &policy(3)).await;

        assert_eq!(result.status, ReconnectStatus::ApiRejected);
        assert_eq!(result.poll_attempts, 0);
        assert_eq!(mock.change_count(), 1);
        assert_eq!(mock.records().len(), 2);
        assert!(result
            .message
            .as_deref()
            .unwrap_or_default()
            .contains("quota exhausted"));
    }

    #[tokio::test]
    async fn background_change_waits_and_retries_current_server_after_rate_limit() {
        let mock = MockServer::start(vec![
            ip(r#"{"ok":true,"ip":"42.1.1.1"}"#),
            rate_limited(unix_now().saturating_sub(2)),
            accepted(),
            ip(r#"{"ok":true,"ip":"42.1.1.2"}"#),
        ])
        .await;
        let client = BoilClient::with_api_base_url(&mock.base_url).unwrap();
        let cooldown_events = Arc::new(Mutex::new(Vec::new()));

        let result = reconnect_one_with_current_ip_progress_cooldown_notify(
            &client,
            &server("rate-limit", true),
            &policy(3),
            None,
            ChangeIpCooldownMode::Wait,
            |_| async {},
            |cooldown| {
                let cooldown_events = Arc::clone(&cooldown_events);
                let change_count_before_wait = mock.change_count();
                async move {
                    cooldown_events
                        .lock()
                        .unwrap()
                        .push((cooldown.remaining, change_count_before_wait));
                }
            },
        )
        .await;

        assert_eq!(result.status, ReconnectStatus::Success);
        assert_eq!(mock.change_count(), 2);
        let cooldown_events = cooldown_events.lock().unwrap();
        assert_eq!(cooldown_events.len(), 1);
        assert!(cooldown_events[0].0 > Duration::ZERO);
        assert_eq!(cooldown_events[0].1, 1);
        assert_eq!(
            mock.records(),
            vec![
                "/api/v1/getIP",
                "/api/v1/changeIP/",
                "/api/v1/changeIP/",
                "/api/v1/getIP",
            ]
        );
    }

    #[tokio::test]
    async fn uncertain_change_response_polls_and_can_confirm_success() {
        let mock = MockServer::start(vec![
            ip(r#"{"ok":true,"ip":"42.1.1.1"}"#),
            disconnect(),
            ip(r#"{"ok":true,"ip":"42.1.1.2"}"#),
        ])
        .await;
        let client = BoilClient::with_api_base_url(&mock.base_url).unwrap();

        let result = reconnect_one(&client, &server("uncertain-change", true), &policy(3)).await;

        assert_eq!(result.status, ReconnectStatus::Success);
        assert!(result.changed);
        assert_eq!(
            result.old_ip.map(|ip| ip.to_string()).as_deref(),
            Some("42.1.1.1")
        );
        assert_eq!(
            result.new_ip.map(|ip| ip.to_string()).as_deref(),
            Some("42.1.1.2")
        );
        assert_eq!(result.poll_attempts, 1);
        assert_eq!(mock.change_count(), 1);
        assert_eq!(mock.request_count(), 3);
    }

    #[tokio::test]
    async fn provided_current_ip_skips_duplicate_preflight_get_ip() {
        let mock = MockServer::start(vec![accepted(), ip(r#"{"ok":true,"ip":"42.1.1.2"}"#)]).await;
        let client = BoilClient::with_api_base_url(&mock.base_url).unwrap();

        let result = reconnect_one_with_current_ip(
            &client,
            &server("prefetched", true),
            &policy(3),
            Some("42.1.1.1".parse().unwrap()),
        )
        .await;

        assert_eq!(result.status, ReconnectStatus::Success);
        assert_eq!(
            mock.records(),
            vec!["/api/v1/changeIP/".to_string(), "/api/v1/getIP".to_string()]
        );
    }

    #[tokio::test]
    async fn accepted_change_with_invalid_poll_is_unconfirmed() {
        let mock = MockServer::start(vec![
            ip(r#"{"ok":true,"ip":"42.1.1.1"}"#),
            accepted(),
            response(200, "not-json"),
        ])
        .await;
        let client = BoilClient::with_api_base_url(&mock.base_url).unwrap();

        let result = reconnect_one(&client, &server("unconfirmed", true), &policy(3)).await;

        assert_eq!(result.status, ReconnectStatus::ChangeAcceptedButUnconfirmed);
        assert_eq!(result.poll_attempts, 3);
        assert_eq!(mock.change_count(), 1);
    }

    #[tokio::test]
    async fn accepted_change_with_network_failure_is_unconfirmed() {
        let mock = MockServer::start(vec![
            ip(r#"{"ok":true,"ip":"42.1.1.1"}"#),
            accepted(),
            disconnect(),
        ])
        .await;
        let client = BoilClient::with_api_base_url(&mock.base_url).unwrap();

        let result = reconnect_one(&client, &server("network", true), &policy(3)).await;

        assert_eq!(result.status, ReconnectStatus::ChangeAcceptedButUnconfirmed);
        assert_eq!(result.poll_attempts, 3);
        assert_eq!(mock.change_count(), 1);
    }

    #[tokio::test]
    async fn invalid_preflight_ip_has_explicit_status_and_no_change() {
        let mock = MockServer::start(vec![ip(r#"{"ok":true,"ip":"invalid"}"#)]).await;
        let client = BoilClient::with_api_base_url(&mock.base_url).unwrap();

        let result = reconnect_one(&client, &server("invalid", true), &policy(1)).await;

        assert_eq!(result.status, ReconnectStatus::InvalidResponse);
        assert_eq!(mock.change_count(), 0);
        assert!(!format!("{result:?}").contains(&test_credential()));
    }

    #[tokio::test]
    async fn disabled_server_makes_no_http_request() {
        let mock = MockServer::start(Vec::new()).await;
        let client = BoilClient::with_api_base_url(&mock.base_url).unwrap();

        let result = reconnect_one(&client, &server("disabled", false), &policy(1)).await;

        assert_eq!(result.status, ReconnectStatus::Disabled);
        assert_eq!(mock.request_count(), 0);
    }

    #[tokio::test]
    async fn unspecified_multiple_servers_fail_before_http() {
        let mock = MockServer::start(Vec::new()).await;
        let client = BoilClient::with_api_base_url(&mock.base_url).unwrap();
        let config = config(vec![server("a", true), server("b", true)]);

        let error = reconnect_selected(&client, &config, ServerSelection::Unspecified, &policy(1))
            .await
            .unwrap_err();

        assert!(error.to_string().contains("必须明确指定"));
        assert_eq!(mock.request_count(), 0);
    }

    #[tokio::test]
    async fn all_with_no_enabled_servers_fails_before_http() {
        let mock = MockServer::start(Vec::new()).await;
        let client = BoilClient::with_api_base_url(&mock.base_url).unwrap();
        let config = config(vec![
            server("disabled-a", false),
            server("disabled-b", false),
        ]);

        let error = reconnect_selected(&client, &config, ServerSelection::All, &policy(1))
            .await
            .unwrap_err();

        assert!(error.to_string().contains("没有已启用"));
        assert_eq!(mock.request_count(), 0);
    }

    #[tokio::test]
    async fn all_runs_in_configuration_order() {
        let mock = MockServer::start(vec![
            ip(r#"{"ok":true,"ip":"42.1.1.1"}"#),
            accepted(),
            ip(r#"{"ok":true,"ip":"42.1.1.2"}"#),
            ip(r#"{"ok":true,"ip":"42.2.2.1"}"#),
            accepted(),
            ip(r#"{"ok":true,"ip":"42.2.2.2"}"#),
        ])
        .await;
        let client = BoilClient::with_api_base_url(&mock.base_url).unwrap();
        let config = config(vec![
            server("a", true),
            server("ignored", false),
            server("b", true),
        ]);

        let batch = reconnect_selected(&client, &config, ServerSelection::All, &policy(1))
            .await
            .unwrap();

        assert_eq!(
            batch
                .results
                .iter()
                .map(|result| result.server_id.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "b"]
        );
        assert_eq!(batch.success_count(), 2);
        assert_eq!(
            mock.records(),
            vec![
                "/api/v1/getIP",
                "/api/v1/changeIP/",
                "/api/v1/getIP",
                "/api/v1/getIP",
                "/api/v1/changeIP/",
                "/api/v1/getIP",
            ]
        );
    }

    #[tokio::test]
    async fn batch_continues_after_partial_failure() {
        let mock = MockServer::start(vec![
            ip(r#"{"ok":true,"ip":"42.1.1.1"}"#),
            accepted(),
            ip(r#"{"ok":true,"ip":"42.1.1.2"}"#),
            ip(r#"{"ok":true,"ip":"42.2.2.1"}"#),
            response(400, r#"{"error":"denied"}"#),
            ip(r#"{"ok":true,"ip":"42.3.3.1"}"#),
            accepted(),
            ip(r#"{"ok":true,"ip":"42.3.3.2"}"#),
        ])
        .await;
        let client = BoilClient::with_api_base_url(&mock.base_url).unwrap();
        let config = config(vec![
            server("a", true),
            server("b", true),
            server("c", true),
        ]);

        let batch = reconnect_selected(&client, &config, ServerSelection::All, &policy(1))
            .await
            .unwrap();

        assert_eq!(batch.results.len(), 3);
        assert_eq!(batch.results[0].status, ReconnectStatus::Success);
        assert_eq!(batch.results[1].status, ReconnectStatus::ApiRejected);
        assert_eq!(batch.results[2].status, ReconnectStatus::Success);
        assert_eq!(batch.success_count(), 2);
        assert_eq!(batch.failure_count(), 1);
    }

    #[tokio::test]
    async fn same_server_reconnects_do_not_overlap_change_requests() {
        let mut delayed = accepted();
        delayed.delay = Duration::from_millis(30);
        let mock = MockServer::start(vec![
            ip(r#"{"ok":true,"ip":"42.1.1.1"}"#),
            delayed,
            ip(r#"{"ok":true,"ip":"42.1.1.2"}"#),
            ip(r#"{"ok":true,"ip":"42.1.1.2"}"#),
            delayed_response(200, r#"{"ok":true,"message":"accepted"}"#),
            ip(r#"{"ok":true,"ip":"42.1.1.3"}"#),
        ])
        .await;
        let client = Arc::new(BoilClient::with_api_base_url(&mock.base_url).unwrap());
        let server = Arc::new(server("concurrency", true));
        let policy = Arc::new(policy(1));

        let first = tokio::spawn({
            let client = Arc::clone(&client);
            let server = Arc::clone(&server);
            let policy = Arc::clone(&policy);
            async move { reconnect_one(&client, &server, &policy).await }
        });
        let second = tokio::spawn({
            let client = Arc::clone(&client);
            let server = Arc::clone(&server);
            let policy = Arc::clone(&policy);
            async move { reconnect_one(&client, &server, &policy).await }
        });

        let (first, second) = tokio::join!(first, second);
        assert_eq!(first.unwrap().status, ReconnectStatus::Success);
        assert_eq!(second.unwrap().status, ReconnectStatus::Success);
        assert_eq!(mock.change_count(), 2);
        assert_eq!(mock.max_active_changes(), 1);
    }

    #[tokio::test]
    async fn change_ip_lock_prevents_overlap_across_different_tasks() {
        let active = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));

        let first = {
            let active = Arc::clone(&active);
            let max_active = Arc::clone(&max_active);
            tokio::spawn(async move {
                with_change_ip_lock(async {
                    let current = active.fetch_add(1, Ordering::SeqCst) + 1;
                    max_active.fetch_max(current, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    active.fetch_sub(1, Ordering::SeqCst);
                })
                .await;
            })
        };
        let second = {
            let active = Arc::clone(&active);
            let max_active = Arc::clone(&max_active);
            tokio::spawn(async move {
                with_change_ip_lock(async {
                    let current = active.fetch_add(1, Ordering::SeqCst) + 1;
                    max_active.fetch_max(current, Ordering::SeqCst);
                    active.fetch_sub(1, Ordering::SeqCst);
                })
                .await;
            })
        };

        first.await.unwrap();
        second.await.unwrap();

        assert_eq!(max_active.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn result_and_messages_do_not_expose_token() {
        let leaked: &'static str =
            Box::leak(format!(r#"{{"error":"{} denied"}}"#, test_credential()).into_boxed_str());
        let mock = MockServer::start(vec![
            ip(r#"{"ok":true,"ip":"42.1.1.1"}"#),
            response(400, leaked),
        ])
        .await;
        let client = BoilClient::with_api_base_url(&mock.base_url).unwrap();

        let result = reconnect_one(&client, &server("redacted", true), &policy(1)).await;
        let rendered = format!(
            "{result:?} {}",
            result.message.as_deref().unwrap_or_default()
        );

        assert!(!rendered.contains(&test_credential()));
        assert!(rendered.contains("<redacted>"));
    }

    #[tokio::test]
    async fn negative_next_allowed_timestamp_is_discarded() {
        let mock = MockServer::start(vec![
            ip(r#"{"ok":true,"ip":"42.1.1.1"}"#),
            response(
                200,
                r#"{"ok":true,"message":"accepted","next_allowed_at":-1}"#,
            ),
            ip(r#"{"ok":true,"ip":"42.1.1.2"}"#),
        ])
        .await;
        let client = BoilClient::with_api_base_url(&mock.base_url).unwrap();

        let result = reconnect_one(&client, &server("timestamp", true), &policy(1)).await;

        assert_eq!(result.status, ReconnectStatus::Success);
        assert_eq!(result.next_allowed_at, None);
    }
}
