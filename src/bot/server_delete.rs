use std::sync::Arc;

use teloxide::{
    prelude::*,
    types::{InlineKeyboardButton, InlineKeyboardMarkup, ParseMode},
};
use tokio::sync::Mutex;

use crate::{config::AppConfig, server_manage::delete_server, timer::TimerManager};

use super::{
    formatting::{format_server_card, html_escape},
    server_edit::save_config_and_reload,
    server_list::find_configured_server,
};

pub(super) async fn show_server_delete_confirmation(
    bot: &Bot,
    chat_id: ChatId,
    config: &AppConfig,
    server_id: &str,
) {
    let server = match find_configured_server(config, server_id) {
        Some(server) => server,
        None => {
            let _ = bot.send_message(chat_id, "服务器不存在或已禁用").await;
            return;
        }
    };
    let keyboard = InlineKeyboardMarkup::new(vec![vec![
        InlineKeyboardButton::callback("✅ 删除", format!("server_delete_confirm:{server_id}")),
        InlineKeyboardButton::callback("❌ 取消", "server_delete_cancel"),
    ]]);
    let _ = bot
        .send_message(
            chat_id,
            format!(
                "⚠️ 确定删除这台服务器吗？\n\n{}",
                format_server_card(server)
            ),
        )
        .reply_markup(keyboard)
        .parse_mode(ParseMode::Html)
        .await;
}

pub(super) async fn delete_server_from_config(
    bot: &Bot,
    chat_id: ChatId,
    config: &Arc<Mutex<AppConfig>>,
    timer: &Arc<Mutex<TimerManager>>,
    server_id: &str,
) {
    let mut next = config.lock().await.clone();
    match delete_server(&mut next, server_id) {
        Ok(()) => {
            save_config_and_reload(bot, chat_id, config, timer, next, "✅ 已删除服务器").await;
        }
        Err(error) => {
            let _ = bot
                .send_message(chat_id, format!("❌ {}", html_escape(&error.to_string())))
                .await;
        }
    }
}
