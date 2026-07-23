use std::{collections::HashSet, sync::Arc, time::Instant};

use teloxide::{
    prelude::*,
    types::{InlineKeyboardButton, InlineKeyboardMarkup, MessageId, ParseMode},
};
use tokio::sync::Mutex;

use crate::{
    config::{save_app_config, AppConfig, SecretToken, ServerConfig},
    timer::TimerManager,
};

use super::{
    formatting::{
        detect_address_metadata, format_server_display_parts, html_escape, normalize_server_address,
    },
    state::{PendingServerDraft, ServerWizardStep, ServerWizardStore, SERVER_WIZARD_TTL},
};

pub(super) async fn start_add_server_wizard(
    bot: &Bot,
    chat_id: ChatId,
    server_wizards: &Arc<Mutex<ServerWizardStore>>,
) {
    server_wizards.lock().await.start(chat_id, Instant::now());
    let _ = bot
        .send_message(chat_id, "请输入服务器名称（必填，支持多行）：")
        .await;
}

pub(super) async fn handle_add_server_input(
    bot: &Bot,
    chat_id: ChatId,
    message_id: MessageId,
    _config: &Arc<Mutex<AppConfig>>,
    _timer: &Arc<Mutex<TimerManager>>,
    server_wizards: &Arc<Mutex<ServerWizardStore>>,
    text: &str,
) -> bool {
    let Some(step) = server_wizards
        .lock()
        .await
        .take_step(chat_id, Instant::now())
    else {
        return false;
    };

    match step {
        ServerWizardStep::Name => {
            let name = text.trim();
            if name.is_empty() {
                server_wizards.lock().await.set_step(
                    chat_id,
                    ServerWizardStep::Name,
                    Instant::now(),
                );
                let _ = bot
                    .send_message(chat_id, "❌ 名称不能为空，请重新输入。")
                    .await;
                return true;
            }
            server_wizards.lock().await.set_step(
                chat_id,
                ServerWizardStep::Address {
                    name: name.to_string(),
                },
                Instant::now(),
            );
            let _ = bot
                .send_message(chat_id, "请输入服务器地址（IP 或域名，不需要 http://）：")
                .await;
        }
        ServerWizardStep::Address { name } => {
            let address = match normalize_server_address(text) {
                Some(address) => address,
                None => {
                    server_wizards.lock().await.set_step(
                        chat_id,
                        ServerWizardStep::Address { name },
                        Instant::now(),
                    );
                    let _ = bot
                        .send_message(chat_id, "❌ 服务器地址不能为空，请重新输入。")
                        .await;
                    return true;
                }
            };
            let metadata = detect_address_metadata(&address).await;
            server_wizards.lock().await.set_step(
                chat_id,
                ServerWizardStep::Token {
                    name,
                    address,
                    geo: metadata.geo,
                    resolved_ip: metadata.resolved_ip,
                },
                Instant::now(),
            );
            let _ = bot.send_message(chat_id, "请输入服务器 Token：").await;
        }
        ServerWizardStep::Token {
            name,
            address,
            geo,
            resolved_ip,
        } => {
            let _ = bot.delete_message(chat_id, message_id).await;
            let token = match SecretToken::new(text.trim().to_string()) {
                Ok(token) => token,
                Err(error) => {
                    server_wizards.lock().await.set_step(
                        chat_id,
                        ServerWizardStep::Token {
                            name,
                            address,
                            geo,
                            resolved_ip,
                        },
                        Instant::now(),
                    );
                    let _ = bot
                        .send_message(chat_id, format!("❌ {}", html_escape(&error.to_string())))
                        .await;
                    return true;
                }
            };
            let draft = PendingServerDraft {
                chat_id,
                name,
                address,
                token,
                geo,
                resolved_ip,
                expires_at: Instant::now() + SERVER_WIZARD_TTL,
            };
            let nonce = server_wizards
                .lock()
                .await
                .insert_draft(draft.clone(), Instant::now());
            show_add_server_confirmation(bot, chat_id, &draft, &nonce).await;
        }
    }

    true
}

async fn show_add_server_confirmation(
    bot: &Bot,
    chat_id: ChatId,
    draft: &PendingServerDraft,
    nonce: &str,
) {
    let keyboard = InlineKeyboardMarkup::new(vec![
        vec![InlineKeyboardButton::callback(
            "✅ 确认添加",
            format!("addserver_confirm:{nonce}"),
        )],
        vec![InlineKeyboardButton::callback(
            "✏️ 重新填写",
            format!("addserver_retry:{nonce}"),
        )],
        vec![InlineKeyboardButton::callback(
            "❌ 取消",
            format!("addserver_cancel:{nonce}"),
        )],
    ]);
    let _ = bot
        .send_message(
            chat_id,
            format!(
                "✅ 服务器验证成功\n\n{}",
                format_server_display_parts(&draft.name, &draft.address, &draft.geo)
            ),
        )
        .reply_markup(keyboard)
        .parse_mode(ParseMode::Html)
        .await;
}

pub(super) async fn confirm_add_server(
    bot: &Bot,
    chat_id: ChatId,
    config: &Arc<Mutex<AppConfig>>,
    timer: &Arc<Mutex<TimerManager>>,
    server_wizards: &Arc<Mutex<ServerWizardStore>>,
    nonce: &str,
) {
    let Some(draft) = server_wizards
        .lock()
        .await
        .take_draft(nonce, Instant::now())
    else {
        let _ = bot
            .send_message(chat_id, "确认已过期，请重新点击添加服务器。")
            .await;
        return;
    };

    let current = config.lock().await.clone();
    let server_id = next_server_id(&current);
    let mut next = current;
    next.servers.push(ServerConfig {
        id: server_id,
        name: draft.name,
        token: draft.token,
        enabled: true,
        address: Some(draft.address),
        country: Some(draft.geo.country),
        flag: Some(draft.geo.flag),
        resolved_ip: draft.resolved_ip,
        timer: None,
    });

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
                    "❌ 已保存服务器，但重新调度失败: {}",
                    html_escape(&error.to_string())
                ),
            )
            .await;
    }
    let _ = bot.send_message(chat_id, "✅ 服务器已添加").await;
}

fn next_server_id(config: &AppConfig) -> String {
    let used = config
        .servers
        .iter()
        .map(|server| server.id.as_str())
        .collect::<HashSet<_>>();
    (1..)
        .map(|index| format!("server-{index}"))
        .find(|id| !used.contains(id.as_str()))
        .expect("unbounded iterator must produce an unused server id")
}

#[cfg(test)]
mod tests {
    #[test]
    fn add_server_callbacks_do_not_contain_token_or_address() {
        let nonce = "nonce-value";
        let confirm = format!("addserver_confirm:{nonce}");
        let retry = format!("addserver_retry:{nonce}");
        let cancel = format!("addserver_cancel:{nonce}");

        assert_eq!(
            super::super::callbacks::parse_callback(&confirm),
            super::super::callbacks::CallbackAction::ConfirmAddServer(nonce)
        );
        assert_eq!(
            super::super::callbacks::parse_callback(&retry),
            super::super::callbacks::CallbackAction::RetryAddServer(nonce)
        );
        assert_eq!(
            super::super::callbacks::parse_callback(&cancel),
            super::super::callbacks::CallbackAction::CancelAddServer(nonce)
        );
        assert!(!confirm.contains("hidden-token"));
        assert!(!confirm.contains("203.0.113.10"));
    }
}
