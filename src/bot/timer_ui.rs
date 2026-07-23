use std::sync::Arc;

use teloxide::{
    prelude::*,
    types::{InlineKeyboardButton, InlineKeyboardMarkup, ParseMode},
};
use tokio::sync::Mutex;

use crate::{
    config::AppConfig,
    timer::{parse_hhmm, TimerManager, TimerStatus, TimerUpdate},
};

use super::{
    formatting::html_escape,
    state::{TimerInputMode, TimerInputStore},
};

pub(super) async fn show_timer_panel(bot: &Bot, chat_id: ChatId, timer: &Arc<Mutex<TimerManager>>) {
    let status = timer.lock().await.status();
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
    let _ = bot
        .send_message(chat_id, format_timer_panel(&status))
        .reply_markup(keyboard)
        .parse_mode(ParseMode::Html)
        .await;
}

pub(super) async fn show_timer_edit_targets(
    bot: &Bot,
    chat_id: ChatId,
    timer: &Arc<Mutex<TimerManager>>,
) {
    let config = timer.lock().await.config().clone();
    let keyboard = timer_target_keyboard(&config, "timer_edit_target");
    let _ = bot
        .send_message(chat_id, "请选择要编辑定时时间的范围：")
        .reply_markup(keyboard)
        .await;
}

pub(super) async fn show_timer_close_targets(
    bot: &Bot,
    chat_id: ChatId,
    timer: &Arc<Mutex<TimerManager>>,
) {
    let config = timer.lock().await.config().clone();
    let keyboard = timer_target_keyboard(&config, "timer_close");
    let _ = bot
        .send_message(chat_id, "请选择要关闭定时换 IP 的范围：")
        .reply_markup(keyboard)
        .await;
}

pub(super) async fn handle_timer_time_input(
    bot: &Bot,
    chat_id: ChatId,
    timer: &Arc<Mutex<TimerManager>>,
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

    if let Err(error) = parse_hhmm(text) {
        let _ = bot
            .send_message(chat_id, format!("❌ {}", html_escape(&error.to_string())))
            .await;
        return;
    }

    match mode {
        TimerInputMode::New => show_timer_create_targets(bot, chat_id, timer, text).await,
        TimerInputMode::Edit(target) => {
            apply_timer_change(
                bot,
                chat_id,
                timer,
                TimerUpdate::Enable {
                    target,
                    hhmm: text.to_string(),
                },
            )
            .await;
        }
    }
}

async fn show_timer_create_targets(
    bot: &Bot,
    chat_id: ChatId,
    timer: &Arc<Mutex<TimerManager>>,
    hhmm: &str,
) {
    let config = timer.lock().await.config().clone();
    let keyboard = timer_create_keyboard(&config, hhmm);
    let _ = bot
        .send_message(chat_id, "请选择定时换 IP 目标：")
        .reply_markup(keyboard)
        .await;
}

pub(super) async fn apply_timer_change(
    bot: &Bot,
    chat_id: ChatId,
    timer: &Arc<Mutex<TimerManager>>,
    update: TimerUpdate,
) {
    let result = timer.lock().await.apply_update(update).await;
    match result {
        Ok(()) => {
            let _ = bot
                .send_message(chat_id, "✅ 定时配置已保存并重新调度")
                .await;
            show_timer_panel(bot, chat_id, timer).await;
        }
        Err(error) => {
            let _ = bot
                .send_message(
                    chat_id,
                    format!("❌ 保存失败: {}", html_escape(&error.to_string())),
                )
                .await;
        }
    }
}

pub(super) fn format_timer_panel(status: &TimerStatus) -> String {
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
            html_escape(server.address.as_deref().unwrap_or("地址未设置")),
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
        let saved_time = time
            .map(|time| format!(" | 保留时间 {}", html_escape(time)))
            .unwrap_or_default();
        format!("已关闭{saved_time}")
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
}
