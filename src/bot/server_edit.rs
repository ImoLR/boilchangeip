use std::{sync::Arc, time::Instant};

use teloxide::{
    prelude::*,
    types::{InlineKeyboardButton, InlineKeyboardMarkup, MessageId, ParseMode},
};
use tokio::sync::Mutex;

use crate::{
    boil::BoilClient,
    config::{save_app_config, AppConfig, SecretToken},
    server_manage::{
        rename_server, update_server_address, update_server_token, ServerAddressUpdate,
    },
    timer::TimerManager,
};

use super::{
    formatting::{
        detect_address_metadata, format_server_card, html_escape, normalize_server_address,
        short_safe_error,
    },
    server_list::find_configured_server,
    state::{ServerEditMode, ServerEditStore},
};

pub(super) async fn handle_server_edit_input(
    bot: &Bot,
    chat_id: ChatId,
    message_id: MessageId,
    config: &Arc<Mutex<AppConfig>>,
    timer: &Arc<Mutex<TimerManager>>,
    server_edits: &Arc<Mutex<ServerEditStore>>,
    text: &str,
) -> bool {
    let Some(mode) = server_edits.lock().await.take(chat_id, Instant::now()) else {
        return false;
    };

    match mode {
        ServerEditMode::Name { server_id } => {
            let mut next = config.lock().await.clone();
            if let Err(error) = rename_server(&mut next, &server_id, text.trim().to_string()) {
                let _ = bot
                    .send_message(chat_id, format!("❌ {}", html_escape(&error.to_string())))
                    .await;
                return true;
            }
            save_config_and_reload(bot, chat_id, config, timer, next, "✅ 服务器名称已更新").await;
        }
        ServerEditMode::Address { server_id } => {
            let Some(address) = normalize_server_address(text) else {
                let _ = bot.send_message(chat_id, "❌ 服务器地址不能为空").await;
                return true;
            };
            let metadata = detect_address_metadata(&address).await;
            let mut next = config.lock().await.clone();
            if let Err(error) = update_server_address(
                &mut next,
                &server_id,
                ServerAddressUpdate {
                    address,
                    country: metadata.geo.country,
                    flag: metadata.geo.flag,
                    resolved_ip: metadata.resolved_ip,
                },
            ) {
                let _ = bot
                    .send_message(chat_id, format!("❌ {}", html_escape(&error.to_string())))
                    .await;
                return true;
            }
            save_config_and_reload(bot, chat_id, config, timer, next, "✅ 服务器地址已更新").await;
        }
        ServerEditMode::Token { server_id } => {
            let _ = bot.delete_message(chat_id, message_id).await;
            let token = match SecretToken::new(text.trim().to_string()) {
                Ok(token) => token,
                Err(error) => {
                    let _ = bot
                        .send_message(chat_id, format!("❌ {}", html_escape(&error.to_string())))
                        .await;
                    return true;
                }
            };
            let client = match BoilClient::new() {
                Ok(client) => client,
                Err(error) => {
                    let _ = bot
                        .send_message(chat_id, format!("❌ 初始化失败: {error}"))
                        .await;
                    return true;
                }
            };
            if let Err(error) = client.get_ip(&token).await {
                let _ = bot
                    .send_message(
                        chat_id,
                        format!(
                            "❌ Token 验证失败: {}",
                            html_escape(&short_safe_error(&error.to_string()))
                        ),
                    )
                    .await;
                return true;
            }

            let mut next = config.lock().await.clone();
            if let Err(error) = update_server_token(&mut next, &server_id, token) {
                let _ = bot
                    .send_message(chat_id, format!("❌ {}", html_escape(&error.to_string())))
                    .await;
                return true;
            }
            save_config_and_reload(bot, chat_id, config, timer, next, "✅ 服务器 Token 已更新")
                .await;
        }
    }
    true
}

pub(super) async fn save_config_and_reload(
    bot: &Bot,
    chat_id: ChatId,
    config: &Arc<Mutex<AppConfig>>,
    timer: &Arc<Mutex<TimerManager>>,
    next: AppConfig,
    success: &str,
) {
    if let Err(error) = save_app_config(&next) {
        let _ = bot
            .send_message(
                chat_id,
                format!("❌ 保存失败: {}", html_escape(&error.to_string())),
            )
            .await;
        return;
    }

    *config.lock().await = next.clone();
    if let Err(error) = timer.lock().await.replace_config(next).await {
        let _ = bot
            .send_message(
                chat_id,
                format!(
                    "❌ 已保存配置，但重新调度失败: {}",
                    html_escape(&error.to_string())
                ),
            )
            .await;
        return;
    }

    let _ = bot.send_message(chat_id, success).await;
}

pub(super) async fn show_server_edit_menu(
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
    let keyboard = InlineKeyboardMarkup::new(vec![
        vec![
            InlineKeyboardButton::callback("✏️ 修改名称", format!("server_edit_name:{server_id}")),
            InlineKeyboardButton::callback(
                "🌍 修改地址",
                format!("server_edit_address:{server_id}"),
            ),
        ],
        vec![
            InlineKeyboardButton::callback(
                "🔑 修改 Token",
                format!("server_edit_token:{server_id}"),
            ),
            InlineKeyboardButton::callback("🔍 重新验证", format!("server_revalidate:{server_id}")),
        ],
        vec![InlineKeyboardButton::callback("⬅️ 返回", "menu:servers")],
    ]);
    let _ = bot
        .send_message(
            chat_id,
            format!("请选择要编辑的项目：\n\n{}", format_server_card(server)),
        )
        .reply_markup(keyboard)
        .parse_mode(ParseMode::Html)
        .await;
}

pub(super) async fn revalidate_server(
    bot: &Bot,
    chat_id: ChatId,
    config: &Arc<Mutex<AppConfig>>,
    timer: &Arc<Mutex<TimerManager>>,
    server_id: &str,
) {
    let current = config.lock().await.clone();
    let server = match find_configured_server(&current, server_id) {
        Some(server) => server,
        None => {
            let _ = bot.send_message(chat_id, "服务器不存在或已禁用").await;
            return;
        }
    };
    let token = server.token.clone();
    let address = server
        .address
        .clone()
        .or_else(|| server.resolved_ip.clone())
        .unwrap_or_else(|| "unknown".to_string());
    let client = match BoilClient::new() {
        Ok(client) => client,
        Err(error) => {
            let _ = bot
                .send_message(chat_id, format!("❌ 初始化失败: {error}"))
                .await;
            return;
        }
    };
    if let Err(error) = client.get_ip(&token).await {
        let _ = bot
            .send_message(
                chat_id,
                format!(
                    "❌ 验证失败: {}",
                    html_escape(&short_safe_error(&error.to_string()))
                ),
            )
            .await;
        return;
    }
    let metadata = detect_address_metadata(&address).await;
    let mut next = current;
    if let Err(error) = update_server_address(
        &mut next,
        server_id,
        ServerAddressUpdate {
            address,
            country: metadata.geo.country,
            flag: metadata.geo.flag,
            resolved_ip: metadata.resolved_ip,
        },
    ) {
        let _ = bot
            .send_message(chat_id, format!("❌ {}", html_escape(&error.to_string())))
            .await;
        return;
    }
    save_config_and_reload(bot, chat_id, config, timer, next, "✅ 服务器已重新验证").await;
}
