use std::sync::Arc;

use teloxide::{
    prelude::*,
    types::{InlineKeyboardButton, InlineKeyboardMarkup, ParseMode},
};
use tokio::sync::Mutex;

use crate::{
    config::{AppConfig, ServerConfig},
    server_manage::{move_server_down, move_server_up},
    timer::TimerManager,
};

use super::{
    commands::start_menu_keyboard,
    formatting::{format_server_card, html_escape},
    server_edit::save_config_and_reload,
};

pub(super) async fn show_servers(bot: &Bot, chat_id: ChatId, config: &AppConfig) {
    if config.servers.is_empty() {
        let _ = bot
            .send_message(chat_id, "尚未添加服务器，请点击“添加服务器”。")
            .reply_markup(start_menu_keyboard())
            .await;
        return;
    }

    for server in &config.servers {
        let _ = bot
            .send_message(chat_id, format_server_card(server))
            .reply_markup(server_manage_keyboard(server))
            .parse_mode(ParseMode::Html)
            .await;
    }
}

pub(super) fn server_manage_keyboard(server: &ServerConfig) -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![
        vec![
            InlineKeyboardButton::callback("📊 状态", format!("select_status:{}", server.id)),
            InlineKeyboardButton::callback("🔄 更换 IP", format!("select_change:{}", server.id)),
        ],
        vec![
            InlineKeyboardButton::callback(
                "⏰ 定时任务",
                format!("timer_edit_target:server:{}", server.id),
            ),
            InlineKeyboardButton::callback("✏️ 编辑", format!("server_edit:{}", server.id)),
        ],
        vec![
            InlineKeyboardButton::callback("🗑 删除", format!("server_delete:{}", server.id)),
            InlineKeyboardButton::callback("⬆️ 上移", format!("server_move_up:{}", server.id)),
            InlineKeyboardButton::callback("⬇️ 下移", format!("server_move_down:{}", server.id)),
        ],
    ])
}

pub(super) fn find_configured_server<'a>(
    config: &'a AppConfig,
    server_id: &str,
) -> Option<&'a ServerConfig> {
    config.servers.iter().find(|server| server.id == server_id)
}

pub(super) async fn move_server(
    bot: &Bot,
    chat_id: ChatId,
    config: &Arc<Mutex<AppConfig>>,
    timer: &Arc<Mutex<TimerManager>>,
    server_id: &str,
    up: bool,
) {
    let mut next = config.lock().await.clone();
    let result = if up {
        move_server_up(&mut next, server_id)
    } else {
        move_server_down(&mut next, server_id)
    };
    match result {
        Ok(()) => {
            save_config_and_reload(bot, chat_id, config, timer, next, "✅ 服务器顺序已更新").await;
        }
        Err(error) => {
            let _ = bot
                .send_message(chat_id, format!("❌ {}", html_escape(&error.to_string())))
                .await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bot::test_support::app_config;

    #[test]
    fn server_manage_keyboard_has_crud_actions_without_token_or_address() {
        let config = app_config();
        let keyboard = server_manage_keyboard(&config.servers[0]);
        let debug = format!("{keyboard:?}");

        assert!(debug.contains("📊 状态"));
        assert!(debug.contains("🔄 更换 IP"));
        assert!(debug.contains("⏰ 定时任务"));
        assert!(debug.contains("编辑"));
        assert!(debug.contains("删除"));
        assert!(debug.contains("上移"));
        assert!(debug.contains("下移"));
        assert!(debug.contains("server_edit:hk-01"));
        assert!(debug.contains("server_delete:hk-01"));
        assert!(!debug.contains("hidden-token"));
        assert!(!debug.contains("203.0.113.10"));
    }
}
