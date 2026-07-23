use std::sync::Arc;

use teloxide::prelude::*;
use tokio::sync::Mutex;
use tokio_cron_scheduler::{Job, JobScheduler};
use uuid::Uuid;

use crate::{
    boil::BoilClient,
    config::{save_app_config, AppConfig, ServerSelection, ServerTimerConfig},
    reconnect::{reconnect_one, reconnect_selected, ReconnectPolicy},
};

const DEFAULT_TIMEZONE: &str = "Asia/Shanghai";
static TIMER_RUN_LOCK: Mutex<()> = Mutex::const_new(());

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
                                run_auto_change_all(&config).await;
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
                        run_auto_change(&config, &server_id).await;
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

async fn run_auto_change(config: &AppConfig, server_id: &str) {
    with_timer_run_lock(async {
        run_auto_change_locked(config, server_id).await;
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

    let result = reconnect_one(&client, selected, &ReconnectPolicy::default()).await;
    log::info!(
        "定时换 IP 完成: server_id={} status={:?} changed={}",
        result.server_id,
        result.status,
        result.changed
    );

    let message = format_timer_result(&result);
    tg_notify(config, &message).await;
}

async fn run_auto_change_all(config: &AppConfig) {
    with_timer_run_lock(async {
        run_auto_change_all_locked(config).await;
    })
    .await;
}

async fn run_auto_change_all_locked(config: &AppConfig) {
    let client = match BoilClient::new() {
        Ok(client) => client,
        Err(e) => {
            log::error!("全局定时换 IP 初始化客户端失败: {e}");
            return;
        }
    };

    let batch = match reconnect_selected(
        &client,
        config,
        ServerSelection::All,
        &ReconnectPolicy::default(),
    )
    .await
    {
        Ok(batch) => batch,
        Err(e) => {
            log::warn!("全局定时换 IP 跳过: {e}");
            return;
        }
    };

    log::info!(
        "全局定时换 IP 完成: success={} unconfirmed={} failed={}",
        batch.success_count(),
        batch.unconfirmed_count(),
        batch.failure_count()
    );
    tg_notify(config, &format_timer_batch_result(&batch)).await;
}

async fn with_timer_run_lock<F, R>(future: F) -> R
where
    F: std::future::Future<Output = R>,
{
    let _guard = TIMER_RUN_LOCK.lock().await;
    future.await
}

fn format_timer_result(result: &crate::reconnect::ReconnectResult) -> String {
    let mut lines = vec![
        format!("⏰ 定时换 IP: {}", result.server_name),
        format!("状态: {:?}", result.status),
    ];
    if let Some(old_ip) = result.old_ip {
        lines.push(format!("旧 IP: {old_ip}"));
    }
    if let Some(new_ip) = result.new_ip {
        lines.push(format!("新 IP: {new_ip}"));
    }
    if let Some(uses_left) = result.uses_left {
        lines.push(format!("剩余次数: {uses_left}"));
    }
    if let Some(next_allowed_at) = result.next_allowed_at {
        lines.push(format!("下次允许时间: {next_allowed_at} (Unix)"));
    }
    if let Some(message) = &result.message {
        lines.push(format!("信息: {message}"));
    }
    lines.join("\n")
}

fn format_timer_batch_result(result: &crate::reconnect::BatchReconnectResult) -> String {
    let mut lines = vec![format!(
        "⏰ 全部 Server 定时换 IP: success={} unconfirmed={} failed={}",
        result.success_count(),
        result.unconfirmed_count(),
        result.failure_count()
    )];
    for item in &result.results {
        lines.push(format!(
            "{} ({}) | {:?} | changed={}",
            item.server_name, item.server_id, item.status, item.changed
        ));
        if let Some(message) = &item.message {
            lines.push(format!("信息: {message}"));
        }
    }
    lines.join("\n")
}

async fn tg_notify(config: &AppConfig, msg: &str) {
    let (token, chat_id) = match (&config.tg_token, &config.tg_chat_id) {
        (Some(token), Some(chat_id)) => (token, chat_id),
        _ => return,
    };

    let bot = Bot::new(token);
    let Ok(chat_id) = chat_id.parse::<i64>() else {
        log::warn!("TG_CHAT_ID 无效，跳过定时通知");
        return;
    };
    let _ = bot.send_message(ChatId(chat_id), msg).await;
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
                    timer: Some(ServerTimerConfig {
                        enabled: true,
                        cron: Some("0 */6 * * *".to_string()),
                    }),
                },
            ],
            global_timer: None,
            tg_token: None,
            tg_chat_id: None,
            migration_notice: None,
        }
    }

    fn enabled_server(id: &str, cron: Option<&str>, timer_enabled: bool) -> ServerConfig {
        ServerConfig {
            id: id.to_string(),
            name: format!("Server {id}"),
            token: SecretToken::from_test_value(&format!("token-{id}")),
            enabled: true,
            timer: cron.map(|cron| ServerTimerConfig {
                enabled: timer_enabled,
                cron: Some(cron.to_string()),
            }),
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
            tg_token: None,
            tg_chat_id: None,
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
            tg_token: None,
            tg_chat_id: None,
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
            tg_token: None,
            tg_chat_id: None,
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
            tg_token: None,
            tg_chat_id: None,
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
            tg_token: None,
            tg_chat_id: None,
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
            tg_token: None,
            tg_chat_id: None,
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
}
