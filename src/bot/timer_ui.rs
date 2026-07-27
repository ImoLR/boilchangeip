use std::{collections::HashMap, sync::Arc};

use teloxide::{
    prelude::*,
    types::{InlineKeyboardButton, InlineKeyboardMarkup, MessageId, ParseMode},
};
use tokio::sync::Mutex;

use crate::{
    boil::BoilClient,
    config::AppConfig,
    timer::{parse_hhmm, TimerManager, TimerStatus, TimerUpdate},
};

use super::{
    formatting::html_escape,
    state::{TimerInputMode, TimerInputStore, TimerMessageStore},
};

pub(super) async fn show_timer_panel(
    bot: &Bot,
    chat_id: ChatId,
    timer: &Arc<Mutex<TimerManager>>,
    timer_messages: &Arc<Mutex<TimerMessageStore>>,
) {
    clear_timer_messages(bot, chat_id, timer_messages).await;
    let (status, config) = {
        let timer = timer.lock().await;
        (timer.status(), timer.config().clone())
    };
    let current_ips = query_timer_current_ips(&config).await;
    let keyboard = InlineKeyboardMarkup::new(vec![
        vec![
            InlineKeyboardButton::callback("➕ 新建", "timer_new"),
            InlineKeyboardButton::callback("✏️ 编辑", "timer_edit"),
        ],
        vec![
            InlineKeyboardButton::callback("⏸ 关闭", "timer_close"),
            InlineKeyboardButton::callback("🔄 刷新", "timer_refresh"),
        ],
    ]);
    let sent = bot
        .send_message(chat_id, format_timer_panel(&status, &current_ips))
        .reply_markup(keyboard)
        .parse_mode(ParseMode::Html)
        .await;
    record_sent_timer_message(chat_id, sent, timer_messages).await;
}

pub(super) async fn show_timer_edit_targets(
    bot: &Bot,
    chat_id: ChatId,
    timer: &Arc<Mutex<TimerManager>>,
    timer_messages: &Arc<Mutex<TimerMessageStore>>,
) {
    let config = timer.lock().await.config().clone();
    let keyboard = timer_target_keyboard(&config, "timer_edit_target");
    let sent = bot
        .send_message(chat_id, "请选择要编辑定时时间的范围：")
        .reply_markup(keyboard)
        .await;
    record_sent_timer_message(chat_id, sent, timer_messages).await;
}

pub(super) async fn show_timer_close_targets(
    bot: &Bot,
    chat_id: ChatId,
    timer: &Arc<Mutex<TimerManager>>,
    timer_messages: &Arc<Mutex<TimerMessageStore>>,
) {
    let config = timer.lock().await.config().clone();
    let keyboard = timer_target_keyboard(&config, "timer_close");
    let sent = bot
        .send_message(chat_id, "请选择要关闭定时换 IP 的范围：")
        .reply_markup(keyboard)
        .await;
    record_sent_timer_message(chat_id, sent, timer_messages).await;
}

pub(super) async fn handle_timer_time_input(
    ctx: TimerUiContext<'_>,
    chat_id: ChatId,
    message_id: MessageId,
    timer_inputs: &Arc<Mutex<TimerInputStore>>,
    text: &str,
) {
    let Some(mode) = timer_inputs
        .lock()
        .await
        .take(chat_id, std::time::Instant::now())
    else {
        return;
    };
    record_timer_message(chat_id, message_id, ctx.timer_messages).await;

    if let Err(error) = parse_hhmm(text) {
        let sent = ctx
            .bot
            .send_message(chat_id, format!("❌ {}", html_escape(&error.to_string())))
            .await;
        record_sent_timer_message(chat_id, sent, ctx.timer_messages).await;
        return;
    }

    match mode {
        TimerInputMode::New => {
            show_timer_create_targets(ctx.bot, chat_id, ctx.timer, ctx.timer_messages, text).await
        }
        TimerInputMode::Edit(target) => {
            apply_timer_change(
                ctx.bot,
                chat_id,
                ctx.config,
                ctx.timer,
                ctx.timer_messages,
                TimerUpdate::Enable {
                    target,
                    hhmm: text.to_string(),
                },
            )
            .await;
        }
    }
}

#[derive(Clone, Copy)]
pub(super) struct TimerUiContext<'a> {
    pub(super) bot: &'a Bot,
    pub(super) config: &'a Arc<Mutex<AppConfig>>,
    pub(super) timer: &'a Arc<Mutex<TimerManager>>,
    pub(super) timer_messages: &'a Arc<Mutex<TimerMessageStore>>,
}

async fn show_timer_create_targets(
    bot: &Bot,
    chat_id: ChatId,
    timer: &Arc<Mutex<TimerManager>>,
    timer_messages: &Arc<Mutex<TimerMessageStore>>,
    hhmm: &str,
) {
    let config = timer.lock().await.config().clone();
    let keyboard = timer_create_keyboard(&config, hhmm);
    let sent = bot
        .send_message(chat_id, "请选择定时换 IP 目标：")
        .reply_markup(keyboard)
        .await;
    record_sent_timer_message(chat_id, sent, timer_messages).await;
}

pub(super) async fn apply_timer_change(
    bot: &Bot,
    chat_id: ChatId,
    config: &Arc<Mutex<AppConfig>>,
    timer: &Arc<Mutex<TimerManager>>,
    timer_messages: &Arc<Mutex<TimerMessageStore>>,
    update: TimerUpdate,
) {
    let result = timer.lock().await.apply_update(update).await;
    match result {
        Ok(()) => {
            let next_config = timer.lock().await.config().clone();
            *config.lock().await = next_config;
            let sent = bot
                .send_message(chat_id, "✅ 定时配置已保存并重新调度")
                .await;
            record_sent_timer_message(chat_id, sent, timer_messages).await;
            show_timer_panel(bot, chat_id, timer, timer_messages).await;
        }
        Err(error) => {
            let sent = bot
                .send_message(
                    chat_id,
                    format!("❌ 保存失败: {}", html_escape(&error.to_string())),
                )
                .await;
            record_sent_timer_message(chat_id, sent, timer_messages).await;
        }
    }
}

async fn query_timer_current_ips(config: &AppConfig) -> HashMap<String, String> {
    let client = match BoilClient::new() {
        Ok(client) => client,
        Err(error) => {
            log::warn!("定时面板初始化 Boil API 客户端失败: {error}");
            return config
                .servers
                .iter()
                .map(|server| (server.id.clone(), "查询失败".to_string()))
                .collect();
        }
    };

    let mut current_ips = HashMap::new();
    for server in &config.servers {
        let current_ip = match client.get_ip(&server.token).await {
            Ok(response) => response.ip.to_string(),
            Err(error) => {
                log::warn!("定时面板查询当前 IP 失败: server_id={}: {error}", server.id);
                "查询失败".to_string()
            }
        };
        current_ips.insert(server.id.clone(), current_ip);
    }
    current_ips
}

pub(super) fn format_timer_panel(
    status: &TimerStatus,
    current_ips: &HashMap<String, String>,
) -> String {
    let mut lines = vec![
        "⏰ <b>定时换 IP</b>".to_string(),
        format!("当前时区: <code>{}</code>", html_escape(status.timezone)),
        format!(
            "🌐 全部 Server: {}",
            timer_state_text(status.global_timer_enabled, status.global_time.as_deref())
        ),
        "Server 定时状态:".to_string(),
    ];

    for server in &status.servers {
        let state = if !server.server_enabled {
            "VPS 已禁用".to_string()
        } else if server.timer_enabled {
            timer_state_text(true, server.time.as_deref())
        } else {
            timer_state_text(false, server.time.as_deref())
        };
        lines.push(format!(
            "\n📡 <b>{}</b>\n\n{} {}\n{}\n{}",
            html_escape(&server.server_name),
            html_escape(server.flag.as_deref().unwrap_or("🌐")),
            html_escape(server.country.as_deref().unwrap_or("未知地区")),
            html_escape(&server_current_ip_text(current_ips.get(&server.server_id))),
            state
        ));
    }

    lines.join("\n")
}

fn timer_state_text(enabled: bool, time: Option<&str>) -> String {
    if enabled {
        format!(
            "已开启 | 每天 {}",
            html_escape(time.unwrap_or("时间未设置"))
        )
    } else {
        "未开启".to_string()
    }
}

fn server_current_ip_text(current_ip: Option<&String>) -> String {
    let value = current_ip.map(String::as_str).unwrap_or("查询失败");
    format!("当前 IP：{value}")
}

pub(super) async fn record_timer_message(
    chat_id: ChatId,
    message_id: MessageId,
    timer_messages: &Arc<Mutex<TimerMessageStore>>,
) {
    timer_messages.lock().await.record(chat_id, message_id);
}

async fn record_sent_timer_message(
    chat_id: ChatId,
    sent: ResponseResult<Message>,
    timer_messages: &Arc<Mutex<TimerMessageStore>>,
) {
    if let Ok(message) = sent {
        record_timer_message(chat_id, message.id, timer_messages).await;
    }
}

async fn clear_timer_messages(
    bot: &Bot,
    chat_id: ChatId,
    timer_messages: &Arc<Mutex<TimerMessageStore>>,
) {
    let messages = timer_messages.lock().await.take_all(chat_id);
    for message_id in messages {
        let _ = bot.delete_message(chat_id, message_id).await;
    }
}

pub(super) fn timer_target_keyboard(config: &AppConfig, prefix: &str) -> InlineKeyboardMarkup {
    let mut rows = vec![vec![InlineKeyboardButton::callback(
        "🌐 全部 Server",
        format!("{prefix}:all"),
    )]];
    rows.extend(enabled_server_buttons(config, prefix));
    InlineKeyboardMarkup::new(rows)
}

pub(super) fn timer_create_keyboard(config: &AppConfig, hhmm: &str) -> InlineKeyboardMarkup {
    let mut rows = vec![vec![InlineKeyboardButton::callback(
        "🌐 全部 Server",
        format!("timer_create:all:{hhmm}"),
    )]];
    rows.extend(enabled_server_buttons_with_time(
        config,
        "timer_create:server",
        hhmm,
    ));
    InlineKeyboardMarkup::new(rows)
}

fn enabled_server_buttons(config: &AppConfig, prefix: &str) -> Vec<Vec<InlineKeyboardButton>> {
    config
        .servers
        .iter()
        .filter(|server| server.enabled)
        .map(|server| {
            vec![InlineKeyboardButton::callback(
                format!("🖥 {}", server.name),
                format!("{prefix}:server:{}", server.id),
            )]
        })
        .collect()
}

fn enabled_server_buttons_with_time(
    config: &AppConfig,
    prefix: &str,
    hhmm: &str,
) -> Vec<Vec<InlineKeyboardButton>> {
    config
        .servers
        .iter()
        .filter(|server| server.enabled)
        .map(|server| {
            vec![InlineKeyboardButton::callback(
                format!("🖥 {}", server.name),
                format!("{prefix}:{}:{hhmm}", server.id),
            )]
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bot::test_support::app_config;

    #[test]
    fn timer_keyboard_has_all_and_single_server_targets() {
        let config = app_config();
        let keyboard = timer_target_keyboard(&config, "timer_close");
        let debug = format!("{keyboard:?}");

        assert!(debug.contains("timer_close:all"));
        assert!(debug.contains("timer_close:server:hk-01"));
        assert!(debug.contains("timer_close:server:jp_02"));
        assert!(!debug.contains("hidden-token"));
    }

    #[test]
    fn timer_panel_uses_not_enabled_without_saved_time_or_missing_address_text() {
        let mut config = app_config();
        config.servers[1].address = None;
        config.servers[1].resolved_ip = Some("198.51.100.20".to_string());
        config.servers[1].timer = Some(crate::config::ServerTimerConfig {
            enabled: false,
            cron: Some("0 5 * * *".to_string()),
        });
        let status = crate::timer::timer_status(&config);
        let current_ips = HashMap::from([("jp_02".to_string(), "203.0.113.200".to_string())]);
        let text = format_timer_panel(&status, &current_ips);

        assert!(text.contains("Japan 02"));
        assert!(text.contains("当前 IP：203.0.113.200"));
        assert!(!text.contains("198.51.100.20"));
        assert!(text.contains("未开启"));
        assert!(!text.contains("地址未设置"));
        assert!(!text.contains("已关闭"));
        assert!(!text.contains("保留时间"));
    }
}
