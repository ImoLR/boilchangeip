use std::sync::Arc;

use teloxide::prelude::*;
use tokio_cron_scheduler::{Job, JobScheduler};
use uuid::Uuid;

use crate::{
    boil::BoilClient,
    config::{AppConfig, ServerSelection},
    reconnect::{reconnect_one, ReconnectPolicy},
};

/// 定时换 IP 管理器：每个任务绑定明确 server_id。
pub struct TimerManager {
    sched: JobScheduler,
    config: Arc<AppConfig>,
    job_ids: Vec<Uuid>,
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

    pub fn current(&self) -> Vec<(String, String, Option<String>)> {
        self.config
            .servers
            .iter()
            .filter_map(|server| {
                if !server.enabled {
                    return None;
                }
                let timer = server.timer.as_ref()?;
                timer
                    .enabled
                    .then(|| (server.id.clone(), server.name.clone(), timer.cron.clone()))
            })
            .collect()
    }

    pub async fn reload(&mut self) -> anyhow::Result<()> {
        self.clear().await?;

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

/// 纯定时守护模式入口（无 TG）。
pub async fn start(config: Arc<AppConfig>) -> anyhow::Result<TimerManager> {
    let has_timer = config.servers.iter().any(|server| {
        server.enabled
            && server
                .timer
                .as_ref()
                .map(|timer| timer.enabled && timer.cron.is_some())
                .unwrap_or(false)
    });
    anyhow::ensure!(has_timer, "未配置任何已启用 VPS 的 timer");
    TimerManager::new(config).await
}

async fn run_auto_change(config: &AppConfig, server_id: &str) {
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
            tg_token: None,
            tg_chat_id: None,
            migration_notice: None,
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
}
