use std::time::{SystemTime, UNIX_EPOCH};

use std::sync::Arc;

use teloxide::{prelude::*, types::ParseMode};
use tokio::sync::Mutex;

use crate::{
    boil::BoilClient,
    config::{AppConfig, ServerConfig},
    timer::parse_hhmm,
};

use super::{
    change::{resolve_for_tg, selected_servers, selection_from_tg_arg},
    formatting::{html_escape, server_geo_label, short_safe_error},
    state::{UiPage, UiSessionStore},
    timer_ui::{record_page_message, record_ui_message},
};

pub(super) async fn tg_status(
    bot: &Bot,
    chat_id: ChatId,
    config: &AppConfig,
    arg: &str,
    ui_sessions: &Arc<Mutex<UiSessionStore>>,
) {
    let selection = selection_from_tg_arg(arg);
    let selected = match resolve_for_tg(bot, chat_id, config, selection, "status").await {
        Some(selected) => selected,
        None => return,
    };

    let client = match BoilClient::new() {
        Ok(client) => client,
        Err(e) => {
            let _ = bot
                .send_message(chat_id, format!("❌ 初始化失败: {e}"))
                .await;
            return;
        }
    };
    let _ = bot
        .send_message(chat_id, "⚙️ 正在查询当前 IP，请稍候…")
        .await;

    for (index, server) in selected_servers(selected).into_iter().enumerate() {
        let (status, detail, current_ip) = match client.get_ip(&server.token).await {
            Ok(response) => (
                StatusQueryState::Normal,
                None,
                Some(response.ip.to_string()),
            ),
            Err(e) => (
                StatusQueryState::VerificationFailed,
                Some(short_safe_error(&e.to_string())),
                None,
            ),
        };
        let text = status_text(config, server, status, current_ip.as_deref(), detail);
        let sent = bot
            .send_message(chat_id, text)
            .parse_mode(ParseMode::Html)
            .await;
        if let Ok(message) = sent {
            if index == 0 {
                record_page_message(bot, chat_id, message.id, UiPage::Status, ui_sessions).await;
            } else {
                record_ui_message(chat_id, message.id, ui_sessions).await;
            }
        }
    }
}

pub(super) fn status_text(
    config: &AppConfig,
    server: &ServerConfig,
    status: StatusQueryState,
    current_ip: Option<&str>,
    detail: Option<String>,
) -> String {
    let status_line = match status {
        StatusQueryState::Normal => "正常".to_string(),
        StatusQueryState::VerificationFailed => detail
            .as_deref()
            .map(|detail| format!("验证失败（{}）", html_escape(detail)))
            .unwrap_or_else(|| "验证失败".to_string()),
    };
    let ip = current_ip.unwrap_or("查询失败");

    format!(
        "✅ 服务器状态\n\n📡 <b>{}</b>\n\n{}\n当前 IP：{}\n\n状态：{}\n下次换 IP：{}",
        html_escape(&server.name),
        html_escape(&server_geo_label(server).display()),
        html_escape(ip),
        status_line,
        html_escape(&next_change_text(config, server, SystemTime::now()))
    )
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum StatusQueryState {
    Normal,
    VerificationFailed,
}

pub(super) fn next_change_text(
    config: &AppConfig,
    server: &ServerConfig,
    now: SystemTime,
) -> String {
    let Some(hhmm) = effective_timer_hhmm(config, server) else {
        return "未开启".to_string();
    };
    next_daily_run_text(&hhmm, now).unwrap_or_else(|| "未开启".to_string())
}

fn effective_timer_hhmm(config: &AppConfig, server: &ServerConfig) -> Option<String> {
    if let Some(timer) = &server.timer {
        if timer.enabled {
            if let Some(hhmm) = timer.cron.as_deref().and_then(crate::timer::cron_to_hhmm) {
                return Some(hhmm);
            }
        }
    }

    if let Some(timer) = &config.global_timer {
        if timer.enabled {
            if let Some(hhmm) = timer.cron.as_deref().and_then(crate::timer::cron_to_hhmm) {
                return Some(hhmm);
            }
        }
    }

    None
}

fn next_daily_run_text(hhmm: &str, now: SystemTime) -> Option<String> {
    let (hour, minute) = parse_hhmm(hhmm).ok()?;
    let now = now.duration_since(UNIX_EPOCH).ok()?.as_secs();
    let shanghai_now = now + shanghai_offset_seconds();
    let seconds_today = shanghai_now % 86_400;
    let target_today = u64::from(hour) * 3_600 + u64::from(minute) * 60;
    let day = if target_today > seconds_today {
        "今天"
    } else {
        "明天"
    };
    Some(format!("{day} {hour:02}:{minute:02}"))
}

fn shanghai_offset_seconds() -> u64 {
    8 * 3_600
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{bot::test_support::app_config, config::ServerTimerConfig};

    #[test]
    fn status_text_uses_public_fields_only_without_action_buttons() {
        let config = app_config();
        let text = status_text(
            &config,
            &config.servers[0],
            StatusQueryState::Normal,
            Some("42.0.0.1"),
            None,
        );

        assert!(text.contains("✅ 服务器状态"));
        assert!(text.contains("Hong Kong 01"));
        assert!(text.contains("中国香港"));
        assert!(text.contains("当前 IP：42.0.0.1"));
        assert!(text.contains("状态：正常"));
        assert!(!text.contains("hk-01"));
        assert!(!text.contains("hidden-token"));
        assert!(!text.contains("更换 IP"));
        assert!(!text.contains("定时任务"));
        assert!(!text.contains("编辑服务器"));
        assert!(!text.contains("删除服务器"));
    }

    #[test]
    fn next_change_text_handles_missing_timer() {
        let mut config = app_config();
        config.global_timer = None;
        config.servers[0].timer = None;
        assert_eq!(
            next_change_text(&config, &config.servers[0], unix_time_at_shanghai(4, 0)),
            "未开启"
        );
    }

    #[test]
    fn next_change_text_uses_single_server_timer_before_global_timer() {
        let mut config = app_config();
        config.global_timer = Some(ServerTimerConfig {
            enabled: true,
            cron: Some("0 5 * * *".to_string()),
        });
        config.servers[0].timer = Some(ServerTimerConfig {
            enabled: true,
            cron: Some("30 3 * * *".to_string()),
        });

        assert_eq!(
            next_change_text(&config, &config.servers[0], unix_time_at_shanghai(2, 0)),
            "今天 03:30"
        );
    }

    #[test]
    fn next_change_text_uses_global_timer_for_single_server_status() {
        let mut config = app_config();
        config.global_timer = Some(ServerTimerConfig {
            enabled: true,
            cron: Some("0 5 * * *".to_string()),
        });
        config.servers[0].timer = None;

        assert_eq!(
            next_change_text(&config, &config.servers[0], unix_time_at_shanghai(4, 0)),
            "今天 05:00"
        );
    }

    #[test]
    fn next_change_text_handles_asia_shanghai_next_day() {
        let mut config = app_config();
        config.global_timer = Some(ServerTimerConfig {
            enabled: true,
            cron: Some("0 5 * * *".to_string()),
        });
        config.servers[0].timer = None;

        assert_eq!(
            next_change_text(&config, &config.servers[0], unix_time_at_shanghai(6, 0)),
            "明天 05:00"
        );
    }

    #[test]
    fn disabled_single_timer_does_not_hide_enabled_global_timer() {
        let mut config = app_config();
        config.global_timer = Some(ServerTimerConfig {
            enabled: true,
            cron: Some("0 5 * * *".to_string()),
        });
        config.servers[0].timer = Some(ServerTimerConfig {
            enabled: false,
            cron: Some("30 3 * * *".to_string()),
        });

        assert_eq!(
            next_change_text(&config, &config.servers[0], unix_time_at_shanghai(4, 0)),
            "今天 05:00"
        );
    }

    fn unix_time_at_shanghai(hour: u64, minute: u64) -> SystemTime {
        let shanghai_seconds = hour * 3_600 + minute * 60;
        UNIX_EPOCH
            + std::time::Duration::from_secs(shanghai_seconds + 86_400 - shanghai_offset_seconds())
    }
}
