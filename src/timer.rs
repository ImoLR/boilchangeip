use std::{
    collections::HashSet,
    sync::{Arc, Mutex as StdMutex, OnceLock},
    time::Duration,
};

use teloxide::prelude::*;
use tokio::sync::Mutex;
use tokio_cron_scheduler::{Job, JobScheduler};
use uuid::Uuid;

use crate::{
    boil::BoilClient,
    config::{
        load_app_config, save_app_config, AppConfig, SecretToken, ServerSelection,
        ServerTimerConfig,
    },
    reconnect::{
        reconnect_one_with_current_ip_progress_cooldown_notify, BatchReconnectResult,
        ChangeIpCooldownMode, ReconnectPolicy, ReconnectProgress, ReconnectStatus,
    },
};

const DEFAULT_TIMEZONE: &str = "Asia/Shanghai";
static TIMER_RUN_LOCK: Mutex<()> = Mutex::const_new(());
static PENDING_TIMER_RETRIES: OnceLock<StdMutex<HashSet<String>>> = OnceLock::new();

const TIMER_RETRY_DELAYS: [Duration; 3] = [
    Duration::from_secs(5 * 60),
    Duration::from_secs(30 * 60),
    Duration::from_secs(60 * 60),
];

/// 定时换 IP 管理器：每个任务绑定明确 server_id。
pub struct TimerManager {
    sched: JobScheduler,
    config: Arc<AppConfig>,
    job_ids: Vec<Uuid>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TimerTarget {
    AllEnabled,
    Server(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TimerUpdate {
    Enable { target: TimerTarget, hhmm: String },
    Disable { target: TimerTarget },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TimerServerStatus {
    pub server_id: String,
    pub server_name: String,
    pub address: Option<String>,
    pub resolved_ip: Option<String>,
    pub country: Option<String>,
    pub flag: Option<String>,
    pub server_enabled: bool,
    pub timer_enabled: bool,
    pub time: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TimerStatus {
    pub timezone: &'static str,
    pub global_timer_enabled: bool,
    pub global_time: Option<String>,
    pub servers: Vec<TimerServerStatus>,
}

impl TimerManager {
    pub async fn new(config: Arc<AppConfig>) -> anyhow::Result<Self> {
        let sched = JobScheduler::new().await?;
        sched.start().await?;
        let mut manager = Self {
            sched,
            config,
            job_ids: Vec::new(),
        };
        manager.reload().await?;
        Ok(manager)
    }

    pub fn status(&self) -> TimerStatus {
        timer_status(&self.config)
    }

    pub fn config(&self) -> &AppConfig {
        &self.config
    }

    #[cfg(test)]
    pub fn job_count(&self) -> usize {
        self.job_ids.len()
    }

    pub async fn apply_update(&mut self, update: TimerUpdate) -> anyhow::Result<()> {
        let mut next = (*self.config).clone();
        apply_timer_update(&mut next, update)?;
        save_app_config(&next)?;
        self.config = Arc::new(next);
        self.reload().await?;
        Ok(())
    }

    pub async fn replace_config(&mut self, config: AppConfig) -> anyhow::Result<()> {
        self.config = Arc::new(config);
        self.reload().await?;
        Ok(())
    }

    pub async fn reload(&mut self) -> anyhow::Result<()> {
        self.clear().await?;

        if let Some(timer) = &self.config.global_timer {
            if timer.enabled {
                if let Some(cron) = timer.cron.as_deref() {
                    let full_expr = format!("0 {}", cron.trim());
                    let config = Arc::clone(&self.config);
                    let job = Job::new_async_tz(
                        &full_expr,
                        chrono_tz::Asia::Shanghai,
                        move |_uuid, _lock| {
                            let config = Arc::clone(&config);
                            Box::pin(async move {
                                run_auto_change_all(Arc::clone(&config)).await;
                            })
                        },
                    )?;

                    self.job_ids.push(self.sched.add(job).await?);
                    log::info!("全局定时换 IP 已生效，cron: {cron}");
                } else {
                    log::warn!("全局定时换 IP 跳过: cron 未设置");
                }
            }
        }

        for server in &self.config.servers {
            if !server.enabled {
                continue;
            }
            let Some(timer) = &server.timer else {
                continue;
            };
            if !timer.enabled {
                continue;
            }
            let Some(cron) = timer.cron.as_deref() else {
                log::warn!("定时换 IP 跳过 {}: cron 未设置", server.id);
                continue;
            };

            let full_expr = format!("0 {}", cron.trim());
            let server_id = server.id.clone();
            let config = Arc::clone(&self.config);
            let job = Job::new_async_tz(
                &full_expr,
                chrono_tz::Asia::Shanghai,
                move |_uuid, _lock| {
                    let config = Arc::clone(&config);
                    let server_id = server_id.clone();
                    Box::pin(async move {
                        run_auto_change(Arc::clone(&config), &server_id).await;
                    })
                },
            )?;

            self.job_ids.push(self.sched.add(job).await?);
            log::info!("定时换 IP 已生效，server_id: {}, cron: {}", server.id, cron);
        }

        Ok(())
    }

    pub async fn clear(&mut self) -> anyhow::Result<()> {
        for id in self.job_ids.drain(..) {
            self.sched.remove(&id).await?;
        }
        Ok(())
    }
}

pub fn timer_status(config: &AppConfig) -> TimerStatus {
    let (global_timer_enabled, global_time) = config
        .global_timer
        .as_ref()
        .map(|timer| (timer.enabled, timer.cron.as_deref().and_then(cron_to_hhmm)))
        .unwrap_or((false, None));

    TimerStatus {
        timezone: DEFAULT_TIMEZONE,
        global_timer_enabled,
        global_time,
        servers: config
            .servers
            .iter()
            .map(|server| {
                let (timer_enabled, time) = server
                    .timer
                    .as_ref()
                    .map(|timer| (timer.enabled, timer.cron.as_deref().and_then(cron_to_hhmm)))
                    .unwrap_or((false, None));
                TimerServerStatus {
                    server_id: server.id.clone(),
                    server_name: server.name.clone(),
                    address: server.address.clone(),
                    resolved_ip: server.resolved_ip.clone(),
                    country: server.country.clone(),
                    flag: server.flag.clone(),
                    server_enabled: server.enabled,
                    timer_enabled,
                    time,
                }
            })
            .collect(),
    }
}

pub fn apply_timer_update(config: &mut AppConfig, update: TimerUpdate) -> anyhow::Result<()> {
    match update {
        TimerUpdate::Enable { target, hhmm } => {
            let cron = daily_cron_from_hhmm(&hhmm)?;
            match target {
                TimerTarget::AllEnabled => {
                    config.global_timer = Some(ServerTimerConfig {
                        enabled: true,
                        cron: Some(cron),
                    });
                }
                TimerTarget::Server(server_id) => {
                    let index = server_index(config, &server_id)?;
                    config.servers[index].timer = Some(ServerTimerConfig {
                        enabled: true,
                        cron: Some(cron),
                    });
                }
            }
        }
        TimerUpdate::Disable { target } => match target {
            TimerTarget::AllEnabled => match &mut config.global_timer {
                Some(timer) => timer.enabled = false,
                None => {
                    config.global_timer = Some(ServerTimerConfig {
                        enabled: false,
                        cron: None,
                    });
                }
            },
            TimerTarget::Server(server_id) => {
                let index = server_index(config, &server_id)?;
                let server = &mut config.servers[index];
                match &mut server.timer {
                    Some(timer) => timer.enabled = false,
                    None => {
                        server.timer = Some(ServerTimerConfig {
                            enabled: false,
                            cron: None,
                        });
                    }
                }
            }
        },
    }
    Ok(())
}

pub fn daily_cron_from_hhmm(input: &str) -> anyhow::Result<String> {
    let (hour, minute) = parse_hhmm(input)?;
    Ok(format!("{minute} {hour} * * *"))
}

pub fn parse_hhmm(input: &str) -> anyhow::Result<(u8, u8)> {
    let trimmed = input.trim();
    let Some((hour, minute)) = trimmed.split_once(':') else {
        anyhow::bail!("时间格式无效，请输入 HH:MM，例如 03:30");
    };
    anyhow::ensure!(
        hour.len() == 2 && minute.len() == 2,
        "时间格式无效，请输入 HH:MM，例如 03:30"
    );
    anyhow::ensure!(
        hour.chars().all(|c| c.is_ascii_digit()) && minute.chars().all(|c| c.is_ascii_digit()),
        "时间格式无效，请输入 HH:MM，例如 03:30"
    );
    let hour: u8 = hour.parse()?;
    let minute: u8 = minute.parse()?;
    anyhow::ensure!(hour < 24, "小时必须在 00-23 之间");
    anyhow::ensure!(minute < 60, "分钟必须在 00-59 之间");
    Ok((hour, minute))
}

pub fn cron_to_hhmm(cron: &str) -> Option<String> {
    let parts = cron.split_whitespace().collect::<Vec<_>>();
    let [minute, hour, "*", "*", "*"] = parts.as_slice() else {
        return None;
    };
    let hour: u8 = hour.parse().ok()?;
    let minute: u8 = minute.parse().ok()?;
    (hour < 24 && minute < 60).then(|| format!("{hour:02}:{minute:02}"))
}

fn server_index(config: &AppConfig, server_id: &str) -> anyhow::Result<usize> {
    let index = config
        .servers
        .iter()
        .position(|server| server.id == server_id)
        .ok_or_else(|| anyhow::anyhow!("未找到 server id: {server_id}"))?;
    anyhow::ensure!(
        config.servers[index].enabled,
        "server id '{server_id}' 已禁用"
    );
    Ok(index)
}

/// 纯定时守护模式入口（无 TG）。
pub async fn start(config: Arc<AppConfig>) -> anyhow::Result<TimerManager> {
    let has_global_timer = config
        .global_timer
        .as_ref()
        .map(|timer| timer.enabled && timer.cron.is_some())
        .unwrap_or(false);
    let has_server_timer = config.servers.iter().any(|server| {
        server.enabled
            && server
                .timer
                .as_ref()
                .map(|timer| timer.enabled && timer.cron.is_some())
                .unwrap_or(false)
    });
    anyhow::ensure!(
        has_global_timer || has_server_timer,
        "未配置任何已启用 VPS 的 timer"
    );
    TimerManager::new(config).await
}

async fn run_auto_change(config: Arc<AppConfig>, server_id: &str) {
    with_timer_run_lock(async {
        run_auto_change_locked(&config, server_id).await;
    })
    .await;
}

async fn run_auto_change_locked(config: &AppConfig, server_id: &str) {
    let selected = match config.resolve_servers(ServerSelection::Id(server_id)) {
        Ok(crate::config::ResolvedSelection::One(server)) => server,
        Ok(crate::config::ResolvedSelection::All(_)) => {
            log::error!("定时换 IP 配置错误: server_id 解析为批量选择");
            return;
        }
        Err(e) => {
            log::warn!("定时换 IP 跳过 server_id={server_id}: {e}");
            return;
        }
    };

    let client = match BoilClient::new() {
        Ok(client) => client,
        Err(e) => {
            log::error!("定时换 IP 初始化客户端失败: {e}");
            return;
        }
    };

    tg_notify(config, "⏰ 定时换 IP 开始\n\n共 1 台 VPS，开始处理...").await;

    let (progress, current_ip) = notify_timer_processing(config, &client, selected).await;
    if current_ip.is_none() {
        tg_notify(config, &timer_preflight_failed_message()).await;
        schedule_timer_preflight_retry(
            selected.id.clone(),
            selected.name.clone(),
            selected.token.clone(),
            TimerRetrySource::SingleServer,
        );
        return;
    }

    let result = reconnect_one_with_current_ip_progress_cooldown_notify(
        &client,
        selected,
        &ReconnectPolicy::default(),
        current_ip,
        ChangeIpCooldownMode::Wait,
        |event| update_timer_progress(progress.clone(), selected.name.clone(), event),
        |cooldown| {
            let message = timer_cooldown_wait_message(&selected.name, cooldown.remaining);
            async move {
                tg_notify(config, &message).await;
            }
        },
    )
    .await;
    log::info!(
        "定时换 IP 完成: server_id={} status={:?} changed={}",
        result.server_id,
        result.status,
        result.changed
    );

    let message = format_timer_result(&result);
    tg_notify(config, &message).await;
}

async fn run_auto_change_all(config: Arc<AppConfig>) {
    with_timer_run_lock(async {
        run_auto_change_all_locked(&config).await;
    })
    .await;
}

async fn run_auto_change_all_locked(config: &AppConfig) {
    let selected = match config.resolve_servers(ServerSelection::All) {
        Ok(crate::config::ResolvedSelection::All(servers)) => servers,
        Ok(crate::config::ResolvedSelection::One(_)) => {
            log::error!("全局定时换 IP 配置错误: 全部 Server 解析为单台选择");
            return;
        }
        Err(e) => {
            log::warn!("全局定时换 IP 跳过: {e}");
            return;
        }
    };

    let client = match BoilClient::new() {
        Ok(client) => client,
        Err(e) => {
            log::error!("全局定时换 IP 初始化客户端失败: {e}");
            return;
        }
    };

    tg_notify(
        config,
        &format!(
            "⏰ 定时换 IP 开始\n\n共 {} 台 VPS，开始逐台处理...",
            selected.len()
        ),
    )
    .await;

    let mut batch = BatchReconnectResult::default();
    for server in selected {
        let (progress, current_ip) = notify_timer_processing(config, &client, server).await;
        if current_ip.is_none() {
            tg_notify(config, &timer_preflight_failed_message()).await;
            schedule_timer_preflight_retry(
                server.id.clone(),
                server.name.clone(),
                server.token.clone(),
                TimerRetrySource::GlobalTimer,
            );
            batch.results.push(timer_preflight_failed_result(server));
            continue;
        }

        let result = reconnect_one_with_current_ip_progress_cooldown_notify(
            &client,
            server,
            &ReconnectPolicy::default(),
            current_ip,
            ChangeIpCooldownMode::Wait,
            |event| update_timer_progress(progress.clone(), server.name.clone(), event),
            |cooldown| {
                let message = timer_cooldown_wait_message(&server.name, cooldown.remaining);
                async move {
                    tg_notify(config, &message).await;
                }
            },
        )
        .await;
        batch.results.push(result);
    }

    log::info!(
        "全局定时换 IP 完成: success={} unconfirmed={} failed={}",
        batch.success_count(),
        batch.unconfirmed_count(),
        batch.failure_count()
    );
    tg_notify(config, &format_timer_batch_result(&batch)).await;
}

async fn notify_timer_processing(
    config: &AppConfig,
    client: &BoilClient,
    server: &crate::config::ServerConfig,
) -> (Option<TimerProgressMessage>, Option<std::net::IpAddr>) {
    let progress = tg_send(config, &format!("🔄 现在处理：{}", server.name)).await;

    let current_ip = match client.get_ip(&server.token).await {
        Ok(response) => Some(response.ip),
        Err(error) => {
            log::warn!(
                "定时换 IP 查询当前 IP 失败: server_id={}: {error}",
                server.id
            );
            None
        }
    };
    (progress, current_ip)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TimerRetrySource {
    SingleServer,
    GlobalTimer,
}

impl TimerRetrySource {
    fn label(self) -> &'static str {
        match self {
            Self::SingleServer => "single",
            Self::GlobalTimer => "global",
        }
    }
}

fn schedule_timer_preflight_retry(
    server_id: String,
    server_name: String,
    original_token: SecretToken,
    source: TimerRetrySource,
) {
    let retry_key = timer_retry_key(&server_id, source);
    if !mark_timer_retry_pending(&retry_key) {
        log::info!("定时换 IP 重试已存在，跳过重复调度: server_id={server_id}");
        return;
    }

    tokio::spawn(async move {
        run_timer_preflight_retry(server_id, server_name, original_token, source, retry_key).await;
    });
}

async fn run_timer_preflight_retry(
    server_id: String,
    _server_name: String,
    original_token: SecretToken,
    source: TimerRetrySource,
    retry_key: String,
) {
    let _pending = TimerRetryPending::new(retry_key);

    for (index, delay) in TIMER_RETRY_DELAYS.iter().enumerate() {
        tokio::time::sleep(*delay).await;

        let _guard = TIMER_RUN_LOCK.lock().await;
        let attempt = TimerRetryAttempt::from_index(index);
        let config = match load_app_config() {
            Ok(config) => config,
            Err(error) => {
                log::warn!("定时换 IP 重试取消: 无法重新读取配置: {error}");
                return;
            }
        };

        let Some(server) =
            retry_server_if_still_valid(&config, &server_id, &original_token, source).cloned()
        else {
            log::info!("定时换 IP 重试取消: server_id={server_id} 已变更或定时任务已关闭");
            return;
        };

        tg_notify(&config, &timer_retry_start_message(attempt, &server.name)).await;

        let client = match BoilClient::new() {
            Ok(client) => client,
            Err(error) => {
                log::error!("定时换 IP 重试初始化客户端失败: {error}");
                return;
            }
        };

        let old_ip = match client.get_ip(&server.token).await {
            Ok(response) => response.ip,
            Err(error) => {
                log::warn!("定时换 IP 重试查询当前 IP 失败: server_id={server_id}: {error}");
                tg_notify(&config, &timer_retry_get_ip_failed_message(attempt)).await;
                if attempt.is_final() {
                    return;
                }
                continue;
            }
        };

        let progress = tg_send(&config, &format!("🔄 现在处理：{}", server.name)).await;
        let result = reconnect_one_with_current_ip_progress_cooldown_notify(
            &client,
            &server,
            &ReconnectPolicy::default(),
            Some(old_ip),
            ChangeIpCooldownMode::Wait,
            |event| update_timer_progress(progress.clone(), server.name.clone(), event),
            |cooldown| {
                let notify_config = config.clone();
                let message = timer_cooldown_wait_message(&server.name, cooldown.remaining);
                async move {
                    tg_notify(&notify_config, &message).await;
                }
            },
        )
        .await;

        if result.status == ReconnectStatus::Success {
            tg_notify(&config, &format_timer_retry_success(&result)).await;
        } else {
            tg_notify(&config, &format_timer_result(&result)).await;
        }
        return;
    }
}

struct TimerRetryPending {
    key: String,
}

impl TimerRetryPending {
    fn new(key: String) -> Self {
        Self { key }
    }
}

impl Drop for TimerRetryPending {
    fn drop(&mut self) {
        clear_timer_retry_pending(&self.key);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TimerRetryAttempt {
    First,
    Second,
    Third,
}

impl TimerRetryAttempt {
    fn from_index(index: usize) -> Self {
        match index {
            0 => Self::First,
            1 => Self::Second,
            _ => Self::Third,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::First => "第一次",
            Self::Second => "第二次",
            Self::Third => "第三次",
        }
    }

    fn next_delay_text(self) -> Option<&'static str> {
        match self {
            Self::First => Some("30 分钟"),
            Self::Second => Some("1 小时"),
            Self::Third => None,
        }
    }

    fn next_attempt_label(self) -> Option<&'static str> {
        match self {
            Self::First => Some("第二次"),
            Self::Second => Some("第三次"),
            Self::Third => None,
        }
    }

    fn is_final(self) -> bool {
        self == Self::Third
    }
}

fn timer_retry_key(server_id: &str, source: TimerRetrySource) -> String {
    format!("{}:{server_id}", source.label())
}

fn pending_timer_retries() -> &'static StdMutex<HashSet<String>> {
    PENDING_TIMER_RETRIES.get_or_init(|| StdMutex::new(HashSet::new()))
}

fn mark_timer_retry_pending(key: &str) -> bool {
    let mut pending = pending_timer_retries()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    pending.insert(key.to_string())
}

fn clear_timer_retry_pending(key: &str) {
    let mut pending = pending_timer_retries()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    pending.remove(key);
}

fn retry_server_if_still_valid<'a>(
    config: &'a AppConfig,
    server_id: &str,
    original_token: &SecretToken,
    source: TimerRetrySource,
) -> Option<&'a crate::config::ServerConfig> {
    let server = config
        .servers
        .iter()
        .find(|server| server.id == server_id && server.enabled)?;
    if server.token.expose_secret() != original_token.expose_secret() {
        return None;
    }

    let timer_enabled = match source {
        TimerRetrySource::SingleServer => server
            .timer
            .as_ref()
            .is_some_and(|timer| timer.enabled && timer.cron.is_some()),
        TimerRetrySource::GlobalTimer => config
            .global_timer
            .as_ref()
            .is_some_and(|timer| timer.enabled && timer.cron.is_some()),
    };
    timer_enabled.then_some(server)
}

fn timer_preflight_failed_result(
    server: &crate::config::ServerConfig,
) -> crate::reconnect::ReconnectResult {
    crate::reconnect::ReconnectResult {
        server_id: server.id.clone(),
        server_name: server.name.clone(),
        old_ip: None,
        new_ip: None,
        changed: false,
        uses_left: None,
        next_allowed_at: None,
        status: ReconnectStatus::PreflightFailed,
        message: Some("换 IP 前查询当前 IP 失败，未发送换 IP 请求".to_string()),
        poll_attempts: 0,
    }
}

fn timer_preflight_failed_message() -> String {
    [
        "❌ 换 IP失败",
        "",
        "原因：",
        "",
        "换 IP 前查询当前 IP 失败，",
        "",
        "未发送换 IP 请求。",
        "",
        "⏳ 将于 5 分钟后开始第一次重新获取 IP。",
    ]
    .join("\n")
}

fn timer_retry_start_message(attempt: TimerRetryAttempt, server_name: &str) -> String {
    format!(
        "🔄 现在开始{}重新获取 IP……\n\n📡 {server_name}",
        attempt.label()
    )
}

fn timer_retry_get_ip_failed_message(attempt: TimerRetryAttempt) -> String {
    if attempt.is_final() {
        return [
            "❌ 第三次重新获取 IP 失败。",
            "",
            "流程结束，",
            "",
            "请管理员手动处理。",
        ]
        .join("\n");
    }

    format!(
        "⚠️ {}重新获取 IP 失败。\n\n⏳ 将于 {}后开始{}重新获取 IP。",
        attempt.label(),
        attempt.next_delay_text().unwrap_or_default(),
        attempt.next_attempt_label().unwrap_or_default()
    )
}

fn timer_cooldown_wait_message(server_name: &str, remaining: Duration) -> String {
    format!(
        "⏳ 换 IP 频率限制中\n\n📡 {server_name}\n\n预计 {}后继续换 IP",
        format_timer_wait_duration(remaining)
    )
}

fn format_timer_wait_duration(duration: Duration) -> String {
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

fn format_timer_retry_success(result: &crate::reconnect::ReconnectResult) -> String {
    let lines = vec![
        "✅ 重试换 IP 成功".to_string(),
        String::new(),
        format!("📡 {}", result.server_name),
        String::new(),
        "旧 IP：".to_string(),
        String::new(),
        result
            .old_ip
            .map(|ip| ip.to_string())
            .unwrap_or_else(|| "N/A".to_string()),
        String::new(),
        "新 IP：".to_string(),
        String::new(),
        result
            .new_ip
            .map(|ip| ip.to_string())
            .unwrap_or_else(|| "N/A".to_string()),
    ];
    lines.join("\n")
}

#[derive(Clone)]
struct TimerProgressMessage {
    bot: Bot,
    chat_id: ChatId,
    message_id: teloxide::types::MessageId,
}

async fn update_timer_progress(
    progress: Option<TimerProgressMessage>,
    server_name: String,
    _event: ReconnectProgress,
) {
    edit_timer_progress(progress, &format!("🔄 现在处理：{server_name}")).await;
}

async fn edit_timer_progress(progress: Option<TimerProgressMessage>, msg: &str) {
    let Some(progress) = progress else {
        return;
    };
    let _ = progress
        .bot
        .edit_message_text(progress.chat_id, progress.message_id, msg)
        .await;
}

async fn with_timer_run_lock<F, R>(future: F) -> R
where
    F: std::future::Future<Output = R>,
{
    let _guard = TIMER_RUN_LOCK.lock().await;
    future.await
}

fn format_timer_result(result: &crate::reconnect::ReconnectResult) -> String {
    let mut lines = vec!["⏰ 定时换 IP 完成".to_string(), String::new()];
    append_timer_result_lines(&mut lines, result);
    lines.join("\n")
}

fn format_timer_batch_result(result: &crate::reconnect::BatchReconnectResult) -> String {
    let mut lines = vec![
        "⏰ 定时换 IP 完成".to_string(),
        String::new(),
        format!("✅ 成功：{}", result.success_count()),
        format!("⚠️ 待确认：{}", result.unconfirmed_count()),
        format!("❌ 失败：{}", result.failure_count()),
    ];

    for item in &result.results {
        lines.push(String::new());
        append_timer_result_lines(&mut lines, item);
    }
    lines.join("\n")
}

fn append_timer_result_lines(lines: &mut Vec<String>, result: &crate::reconnect::ReconnectResult) {
    lines.push(format!("📡 {}", result.server_name));
    lines.push(timer_status_message(result).to_string());
    if let Some(old_ip) = result.old_ip {
        lines.push(format!("旧 IP：{old_ip}"));
    }
    if let Some(new_ip) = result.new_ip {
        lines.push(format!("新 IP：{new_ip}"));
    }
    if let Some(uses_left) = result.uses_left {
        lines.push(format!("剩余次数：{uses_left}"));
    }
    if result.status != ReconnectStatus::Success {
        lines.push(format!("原因：{}", timer_reason(result)));
    }
}

fn timer_status_message(result: &crate::reconnect::ReconnectResult) -> &'static str {
    match result.status {
        ReconnectStatus::Success => "✅ 换 IP 成功",
        ReconnectStatus::RateLimited => "⏳ 换 IP 频率限制中",
        ReconnectStatus::ChangeAcceptedButUnconfirmed => "⚠️ 换 IP 已提交，但暂时无法确认结果",
        _ => "❌ 换 IP 失败",
    }
}

fn timer_reason(result: &crate::reconnect::ReconnectResult) -> String {
    match result.status {
        ReconnectStatus::Success => "换 IP 已完成".to_string(),
        ReconnectStatus::Disabled => "这台 VPS 已禁用".to_string(),
        ReconnectStatus::PreflightFailed => {
            "换 IP 前查询当前 IP 失败，未发送换 IP 请求".to_string()
        }
        ReconnectStatus::RateLimited => result
            .message
            .clone()
            .unwrap_or_else(|| "换 IP 频率限制中，请稍后再试".to_string()),
        ReconnectStatus::ApiRejected => result
            .message
            .clone()
            .unwrap_or_else(|| "Boil API 拒绝请求".to_string()),
        ReconnectStatus::ChangeAcceptedButUnconfirmed => {
            "查询新 IP 超时或暂时失败，请稍后使用 /status 查看".to_string()
        }
        ReconnectStatus::InvalidResponse => "Boil API 响应无效".to_string(),
    }
}

async fn tg_notify(config: &AppConfig, msg: &str) {
    let _ = tg_send(config, msg).await;
}

async fn tg_send(config: &AppConfig, msg: &str) -> Option<TimerProgressMessage> {
    let (token, chat_id) = match (&config.tg_token, &config.tg_chat_id) {
        (Some(token), Some(chat_id)) => (token, chat_id),
        _ => return None,
    };

    let bot = Bot::new(token);
    let Ok(chat_id) = chat_id.parse::<i64>() else {
        log::warn!("TG_CHAT_ID 无效，跳过定时通知");
        return None;
    };
    let chat_id = ChatId(chat_id);
    match bot.send_message(chat_id, msg).await {
        Ok(message) => Some(TimerProgressMessage {
            bot,
            chat_id,
            message_id: message.id,
        }),
        Err(error) => {
            log::warn!("发送定时通知失败: {error}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{SecretToken, ServerConfig, ServerTimerConfig};

    fn app_config() -> AppConfig {
        AppConfig {
            servers: vec![
                ServerConfig {
                    id: "a".to_string(),
                    name: "A".to_string(),
                    token: SecretToken::from_test_value("token-a"),
                    enabled: true,
                    address: None,
                    country: None,
                    flag: None,
                    resolved_ip: None,
                    timer: Some(ServerTimerConfig {
                        enabled: true,
                        cron: Some("0 */6 * * *".to_string()),
                    }),
                },
                ServerConfig {
                    id: "b".to_string(),
                    name: "B".to_string(),
                    token: SecretToken::from_test_value("token-b"),
                    enabled: false,
                    address: None,
                    country: None,
                    flag: None,
                    resolved_ip: None,
                    timer: Some(ServerTimerConfig {
                        enabled: true,
                        cron: Some("0 */6 * * *".to_string()),
                    }),
                },
            ],
            global_timer: None,
            change_ip_cooldown: None,
            tg_token: None,
            tg_chat_id: None,
            tg_pair_code: None,
            tg_pair_expires_at: None,
            migration_notice: None,
        }
    }

    fn enabled_server(id: &str, cron: Option<&str>, timer_enabled: bool) -> ServerConfig {
        ServerConfig {
            id: id.to_string(),
            name: format!("Server {id}"),
            token: SecretToken::from_test_value(&format!("token-{id}")),
            enabled: true,
            address: None,
            country: None,
            flag: None,
            resolved_ip: None,
            timer: cron.map(|cron| ServerTimerConfig {
                enabled: timer_enabled,
                cron: Some(cron.to_string()),
            }),
        }
    }

    fn reconnect_result(name: &str, status: ReconnectStatus) -> crate::reconnect::ReconnectResult {
        crate::reconnect::ReconnectResult {
            server_id: name.to_lowercase(),
            server_name: name.to_string(),
            old_ip: Some("42.1.1.1".parse().unwrap()),
            new_ip: (status == ReconnectStatus::Success).then(|| "42.1.1.2".parse().unwrap()),
            changed: status == ReconnectStatus::Success,
            uses_left: (status == ReconnectStatus::Success).then_some(997),
            next_allowed_at: Some(1785100089),
            status,
            message: Some("Boil API 请求超时".to_string()),
            poll_attempts: 3,
        }
    }

    #[test]
    fn current_lists_only_enabled_timer_entries() {
        let config = Arc::new(app_config());
        let sched = futures_test_scheduler_placeholder(config);
        assert_eq!(sched.len(), 1);
        assert_eq!(
            sched.iter().map(|item| item.0.as_str()).collect::<Vec<_>>(),
            vec!["a"]
        );
    }

    fn futures_test_scheduler_placeholder(
        config: Arc<AppConfig>,
    ) -> Vec<(String, String, Option<String>)> {
        config
            .servers
            .iter()
            .filter_map(|server| {
                let timer = server.timer.as_ref()?;
                (server.enabled && timer.enabled)
                    .then(|| (server.id.clone(), server.name.clone(), timer.cron.clone()))
            })
            .collect()
    }

    #[test]
    fn missing_timer_server_does_not_fallback() {
        let config = app_config();
        let error = config
            .resolve_servers(ServerSelection::Id("missing"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("未找到 server id"));
        assert!(!error.contains("token-a"));
    }

    #[test]
    fn hhmm_accepts_valid_daily_times() {
        assert_eq!(parse_hhmm("00:00").unwrap(), (0, 0));
        assert_eq!(parse_hhmm("23:59").unwrap(), (23, 59));
        assert_eq!(daily_cron_from_hhmm("03:30").unwrap(), "30 3 * * *");
        assert_eq!(cron_to_hhmm("30 3 * * *").as_deref(), Some("03:30"));
    }

    #[test]
    fn hhmm_rejects_invalid_times() {
        for input in ["3:30", "24:00", "12:60", "aa:bb", "12-30"] {
            assert!(parse_hhmm(input).is_err(), "{input} should be rejected");
        }
    }

    #[test]
    fn global_timer_and_single_server_timers_can_coexist() {
        let mut config = app_config();
        apply_timer_update(
            &mut config,
            TimerUpdate::Enable {
                target: TimerTarget::AllEnabled,
                hhmm: "03:30".to_string(),
            },
        )
        .unwrap();

        assert_eq!(
            config
                .global_timer
                .as_ref()
                .and_then(|timer| timer.cron.as_deref()),
            Some("30 3 * * *")
        );
        assert_eq!(
            config.servers[0]
                .timer
                .as_ref()
                .and_then(|timer| timer.cron.as_deref()),
            Some("0 */6 * * *")
        );
        assert_eq!(
            config.servers[1]
                .timer
                .as_ref()
                .and_then(|timer| timer.cron.as_deref()),
            Some("0 */6 * * *")
        );
    }

    #[test]
    fn setting_global_timer_does_not_override_single_server_timer() {
        let mut config = AppConfig {
            servers: vec![enabled_server("a", Some("0 8 * * *"), true)],
            global_timer: None,
            change_ip_cooldown: None,
            tg_token: None,
            tg_chat_id: None,
            tg_pair_code: None,
            tg_pair_expires_at: None,
            migration_notice: None,
        };

        apply_timer_update(
            &mut config,
            TimerUpdate::Enable {
                target: TimerTarget::AllEnabled,
                hhmm: "03:30".to_string(),
            },
        )
        .unwrap();

        assert_eq!(
            config
                .global_timer
                .as_ref()
                .and_then(|timer| timer.cron.as_deref()),
            Some("30 3 * * *")
        );
        assert_eq!(
            config.servers[0]
                .timer
                .as_ref()
                .and_then(|timer| timer.cron.as_deref()),
            Some("0 8 * * *")
        );
    }

    #[test]
    fn single_server_timer_update_changes_only_that_server() {
        let mut config = AppConfig {
            servers: vec![
                enabled_server("a", Some("0 1 * * *"), true),
                enabled_server("b", Some("0 2 * * *"), true),
            ],
            global_timer: Some(ServerTimerConfig {
                enabled: true,
                cron: Some("30 3 * * *".to_string()),
            }),
            change_ip_cooldown: None,
            tg_token: None,
            tg_chat_id: None,
            tg_pair_code: None,
            tg_pair_expires_at: None,
            migration_notice: None,
        };

        apply_timer_update(
            &mut config,
            TimerUpdate::Enable {
                target: TimerTarget::Server("b".to_string()),
                hhmm: "04:45".to_string(),
            },
        )
        .unwrap();

        assert_eq!(
            config.servers[0]
                .timer
                .as_ref()
                .and_then(|timer| timer.cron.as_deref()),
            Some("0 1 * * *")
        );
        assert_eq!(
            config.servers[1]
                .timer
                .as_ref()
                .and_then(|timer| timer.cron.as_deref()),
            Some("45 4 * * *")
        );
        assert_eq!(
            config
                .global_timer
                .as_ref()
                .and_then(|timer| timer.cron.as_deref()),
            Some("30 3 * * *")
        );
    }

    #[test]
    fn disabling_timer_preserves_time() {
        let mut config = AppConfig {
            servers: vec![enabled_server("a", Some("30 3 * * *"), true)],
            global_timer: Some(ServerTimerConfig {
                enabled: true,
                cron: Some("0 1 * * *".to_string()),
            }),
            change_ip_cooldown: None,
            tg_token: None,
            tg_chat_id: None,
            tg_pair_code: None,
            tg_pair_expires_at: None,
            migration_notice: None,
        };

        apply_timer_update(
            &mut config,
            TimerUpdate::Disable {
                target: TimerTarget::Server("a".to_string()),
            },
        )
        .unwrap();

        let timer = config.servers[0].timer.as_ref().unwrap();
        assert!(!timer.enabled);
        assert_eq!(timer.cron.as_deref(), Some("30 3 * * *"));
        assert!(config.global_timer.as_ref().unwrap().enabled);
    }

    #[test]
    fn disabling_global_timer_preserves_single_server_timers() {
        let mut config = AppConfig {
            servers: vec![enabled_server("a", Some("0 8 * * *"), true)],
            global_timer: Some(ServerTimerConfig {
                enabled: true,
                cron: Some("30 3 * * *".to_string()),
            }),
            change_ip_cooldown: None,
            tg_token: None,
            tg_chat_id: None,
            tg_pair_code: None,
            tg_pair_expires_at: None,
            migration_notice: None,
        };

        apply_timer_update(
            &mut config,
            TimerUpdate::Disable {
                target: TimerTarget::AllEnabled,
            },
        )
        .unwrap();

        let global = config.global_timer.as_ref().unwrap();
        assert!(!global.enabled);
        assert_eq!(global.cron.as_deref(), Some("30 3 * * *"));
        assert!(config.servers[0].timer.as_ref().unwrap().enabled);
    }

    #[test]
    fn timer_status_restores_global_and_single_timers_after_reload() {
        let config = AppConfig {
            servers: vec![enabled_server("a", Some("30 3 * * *"), true)],
            global_timer: Some(ServerTimerConfig {
                enabled: true,
                cron: Some("45 4 * * *".to_string()),
            }),
            change_ip_cooldown: None,
            tg_token: None,
            tg_chat_id: None,
            tg_pair_code: None,
            tg_pair_expires_at: None,
            migration_notice: None,
        };

        let serialized = serde_json::to_string(&config.servers).unwrap();
        let global_timer = serde_json::to_string(&config.global_timer).unwrap();
        let reloaded = AppConfig::from_env_vars([
            ("BOIL_SERVERS", serialized.as_str()),
            ("BOIL_GLOBAL_TIMER", global_timer.as_str()),
        ])
        .unwrap();
        let status = timer_status(&reloaded);

        assert_eq!(status.timezone, "Asia/Shanghai");
        assert_eq!(status.global_time.as_deref(), Some("04:45"));
        assert!(status.global_timer_enabled);
        assert_eq!(status.servers[0].time.as_deref(), Some("03:30"));
        assert!(status.servers[0].timer_enabled);
    }

    #[tokio::test]
    async fn reload_replaces_global_and_single_jobs_without_duplicates() {
        let config = Arc::new(AppConfig {
            servers: vec![
                enabled_server("a", Some("59 23 * * *"), true),
                enabled_server("b", Some("58 23 * * *"), true),
            ],
            global_timer: Some(ServerTimerConfig {
                enabled: true,
                cron: Some("57 23 * * *".to_string()),
            }),
            change_ip_cooldown: None,
            tg_token: None,
            tg_chat_id: None,
            tg_pair_code: None,
            tg_pair_expires_at: None,
            migration_notice: None,
        });
        let mut manager = TimerManager::new(config).await.unwrap();

        assert_eq!(manager.job_count(), 3);
        manager.reload().await.unwrap();
        assert_eq!(manager.job_count(), 3);
    }

    #[tokio::test]
    async fn timer_workers_do_not_overlap() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let active = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));

        let first = {
            let active = Arc::clone(&active);
            let max_active = Arc::clone(&max_active);
            tokio::spawn(async move {
                with_timer_run_lock(async {
                    let current = active.fetch_add(1, Ordering::SeqCst) + 1;
                    max_active.fetch_max(current, Ordering::SeqCst);
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                    active.fetch_sub(1, Ordering::SeqCst);
                })
                .await;
            })
        };
        let second = {
            let active = Arc::clone(&active);
            let max_active = Arc::clone(&max_active);
            tokio::spawn(async move {
                with_timer_run_lock(async {
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

    #[test]
    fn timer_single_result_uses_human_readable_text() {
        let text = format_timer_result(&reconnect_result("Taiwan VPS", ReconnectStatus::Success));

        assert!(text.contains("⏰ 定时换 IP 完成"));
        assert!(text.contains("📡 Taiwan VPS"));
        assert!(text.contains("✅ 换 IP 成功"));
        assert!(text.contains("旧 IP：42.1.1.1"));
        assert!(text.contains("新 IP：42.1.1.2"));
        assert!(text.contains("剩余次数：997"));
        assert!(!text.contains("Success"));
        assert!(!text.contains("changed=false"));
        assert!(!text.contains("1785100089"));
    }

    #[test]
    fn timer_batch_result_summarizes_success_unconfirmed_and_failed() {
        let batch = crate::reconnect::BatchReconnectResult {
            results: vec![
                reconnect_result("Taiwan VPS", ReconnectStatus::Success),
                reconnect_result("Hong Kong VPS", ReconnectStatus::ApiRejected),
                reconnect_result("Japan VPS", ReconnectStatus::ChangeAcceptedButUnconfirmed),
            ],
        };

        let text = format_timer_batch_result(&batch);

        assert!(text.contains("✅ 成功：1"));
        assert!(text.contains("⚠️ 待确认：1"));
        assert!(text.contains("❌ 失败：1"));
        assert!(text.contains("📡 Taiwan VPS"));
        assert!(text.contains("📡 Hong Kong VPS"));
        assert!(text.contains("📡 Japan VPS"));
        assert!(text.contains("⚠️ 换 IP 已提交，但暂时无法确认结果"));
        assert!(text.contains("原因：查询新 IP 超时或暂时失败，请稍后使用 /status 查看"));
        assert!(!text.contains("success="));
        assert!(!text.contains("NetworkError"));
        assert!(!text.contains("PreflightFailed"));
        assert!(!text.contains("changed=false"));
    }

    #[test]
    fn timer_preflight_retry_messages_match_background_retry_flow() {
        assert_eq!(
            timer_preflight_failed_message(),
            [
                "❌ 换 IP失败",
                "",
                "原因：",
                "",
                "换 IP 前查询当前 IP 失败，",
                "",
                "未发送换 IP 请求。",
                "",
                "⏳ 将于 5 分钟后开始第一次重新获取 IP。",
            ]
            .join("\n")
        );
        assert_eq!(
            timer_retry_start_message(TimerRetryAttempt::First, "Taiwan VPS"),
            "🔄 现在开始第一次重新获取 IP……\n\n📡 Taiwan VPS"
        );
        assert_eq!(
            timer_retry_get_ip_failed_message(TimerRetryAttempt::First),
            "⚠️ 第一次重新获取 IP 失败。\n\n⏳ 将于 30 分钟后开始第二次重新获取 IP。"
        );
        assert_eq!(
            timer_retry_get_ip_failed_message(TimerRetryAttempt::Second),
            "⚠️ 第二次重新获取 IP 失败。\n\n⏳ 将于 1 小时后开始第三次重新获取 IP。"
        );
        assert_eq!(
            timer_retry_get_ip_failed_message(TimerRetryAttempt::Third),
            "❌ 第三次重新获取 IP 失败。\n\n流程结束，\n\n请管理员手动处理。"
        );
    }

    #[test]
    fn timer_cooldown_wait_message_says_background_will_continue() {
        assert_eq!(
            timer_cooldown_wait_message("Taiwan VPS", Duration::from_secs(59)),
            "⏳ 换 IP 频率限制中\n\n📡 Taiwan VPS\n\n预计 59 秒后继续换 IP"
        );
        assert_eq!(
            timer_cooldown_wait_message("Taiwan VPS", Duration::from_secs(61)),
            "⏳ 换 IP 频率限制中\n\n📡 Taiwan VPS\n\n预计 2 分钟后继续换 IP"
        );
    }

    #[test]
    fn timer_retry_success_message_hides_internal_details() {
        let text =
            format_timer_retry_success(&reconnect_result("Taiwan VPS", ReconnectStatus::Success));

        assert!(text.contains("✅ 重试换 IP 成功"));
        assert!(text.contains("📡 Taiwan VPS"));
        assert!(text.contains("旧 IP：\n\n42.1.1.1"));
        assert!(text.contains("新 IP：\n\n42.1.1.2"));
        assert!(!text.contains("剩余次数"));
        assert!(!text.contains("997"));
        assert!(!text.contains("Success"));
        assert!(!text.contains("Boil API"));
    }

    #[test]
    fn timer_retry_registry_rejects_duplicate_pending_retry() {
        let key = "test-duplicate-retry:a";
        clear_timer_retry_pending(key);

        assert!(mark_timer_retry_pending(key));
        assert!(!mark_timer_retry_pending(key));
        clear_timer_retry_pending(key);
        assert!(mark_timer_retry_pending(key));
        clear_timer_retry_pending(key);
    }

    #[test]
    fn timer_retry_recheck_requires_server_token_and_timer_to_match() {
        let original = SecretToken::from_test_value("token-a");
        let mut config = app_config();

        assert!(retry_server_if_still_valid(
            &config,
            "a",
            &original,
            TimerRetrySource::SingleServer
        )
        .is_some());

        config.servers[0].enabled = false;
        assert!(retry_server_if_still_valid(
            &config,
            "a",
            &original,
            TimerRetrySource::SingleServer
        )
        .is_none());

        config = app_config();
        config.servers[0].token = SecretToken::from_test_value("new-token-a");
        assert!(retry_server_if_still_valid(
            &config,
            "a",
            &original,
            TimerRetrySource::SingleServer
        )
        .is_none());

        config = app_config();
        config.servers[0].timer.as_mut().unwrap().enabled = false;
        assert!(retry_server_if_still_valid(
            &config,
            "a",
            &original,
            TimerRetrySource::SingleServer
        )
        .is_none());
    }

    #[test]
    fn timer_retry_recheck_uses_global_timer_for_global_retry() {
        let original = SecretToken::from_test_value("token-a");
        let mut config = app_config();
        config.global_timer = Some(ServerTimerConfig {
            enabled: true,
            cron: Some("0 3 * * *".to_string()),
        });
        config.servers[0].timer = None;

        assert!(retry_server_if_still_valid(
            &config,
            "a",
            &original,
            TimerRetrySource::GlobalTimer
        )
        .is_some());

        config.global_timer.as_mut().unwrap().enabled = false;
        assert!(retry_server_if_still_valid(
            &config,
            "a",
            &original,
            TimerRetrySource::GlobalTimer
        )
        .is_none());
    }
}
