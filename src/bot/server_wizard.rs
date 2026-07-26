use std::sync::Arc;
use std::time::Instant;

use teloxide::{
    prelude::*,
    types::{InlineKeyboardButton, InlineKeyboardMarkup, MessageId, ParseMode},
};
use tokio::sync::Mutex;

use crate::{
    boil::BoilClient,
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
        .send_message(chat_id, "请发送这台 VPS 的新版 Boil Token：")
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
        ServerWizardStep::Token => {
            handle_token_step(bot, chat_id, message_id, server_wizards, text).await;
        }
        ServerWizardStep::Name { current_ip, token } => {
            handle_name_step(bot, chat_id, server_wizards, text, current_ip, token).await;
        }
        ServerWizardStep::Address {
            name,
            current_ip,
            token,
            geo,
            resolved_ip,
        } => {
            handle_address_step(
                bot,
                chat_id,
                server_wizards,
                text,
                AddressStepData {
                    name,
                    current_ip,
                    token,
                    geo,
                    resolved_ip,
                },
            )
            .await;
        }
    }

    true
}

async fn handle_token_step(
    bot: &Bot,
    chat_id: ChatId,
    message_id: MessageId,
    server_wizards: &Arc<Mutex<ServerWizardStore>>,
    text: &str,
) {
    let _ = bot.delete_message(chat_id, message_id).await;
    let token = match SecretToken::new(text.trim().to_string()) {
        Ok(token) => token,
        Err(_) => {
            reset_to_token_step(bot, chat_id, server_wizards).await;
            return;
        }
    };
    let client = match BoilClient::new() {
        Ok(client) => client,
        Err(error) => {
            server_wizards
                .lock()
                .await
                .set_step(chat_id, ServerWizardStep::Token, Instant::now());
            let _ = bot
                .send_message(chat_id, format!("❌ 初始化失败: {error}"))
                .await;
            return;
        }
    };
    let current_ip = match client.get_ip(&token).await {
        Ok(response) => response.ip.to_string(),
        Err(_) => {
            reset_to_token_step(bot, chat_id, server_wizards).await;
            return;
        }
    };

    server_wizards.lock().await.set_step(
        chat_id,
        ServerWizardStep::Name {
            current_ip: current_ip.clone(),
            token,
        },
        Instant::now(),
    );
    let _ = bot
        .send_message(
            chat_id,
            format!(
                "✅ Token 验证成功\n\n当前 IP：{}\n\n请给这台服务器设置一个显示名称，例如 HKT、HKG、Tokyo、JP、US：",
                html_escape(&current_ip)
            ),
        )
        .await;
}

async fn reset_to_token_step(
    bot: &Bot,
    chat_id: ChatId,
    server_wizards: &Arc<Mutex<ServerWizardStore>>,
) {
    server_wizards
        .lock()
        .await
        .set_step(chat_id, ServerWizardStep::Token, Instant::now());
    let _ = bot
        .send_message(chat_id, "❌ Token 验证失败\n\n请重新发送新版 Boil Token。")
        .await;
}

async fn handle_name_step(
    bot: &Bot,
    chat_id: ChatId,
    server_wizards: &Arc<Mutex<ServerWizardStore>>,
    text: &str,
    current_ip: String,
    token: SecretToken,
) {
    let name = text.trim();
    if name.is_empty() {
        server_wizards.lock().await.set_step(
            chat_id,
            ServerWizardStep::Name { current_ip, token },
            Instant::now(),
        );
        let _ = bot
            .send_message(chat_id, "❌ 名称不能为空，请重新输入。")
            .await;
        return;
    }

    let metadata = detect_address_metadata(&current_ip).await;
    server_wizards.lock().await.set_step(
        chat_id,
        ServerWizardStep::Address {
            name: name.to_string(),
            current_ip,
            token,
            geo: metadata.geo,
            resolved_ip: metadata.resolved_ip,
        },
        Instant::now(),
    );
    let _ = bot
        .send_message(
            chat_id,
            "是否填写服务器域名/地址？\n\n可以直接发送 IP 或域名，例如 boil.example.com。\n不填写请发送 /skip。",
        )
        .await;
}

struct AddressStepData {
    name: String,
    current_ip: String,
    token: SecretToken,
    geo: super::formatting::GeoLabel,
    resolved_ip: Option<String>,
}

async fn handle_address_step(
    bot: &Bot,
    chat_id: ChatId,
    server_wizards: &Arc<Mutex<ServerWizardStore>>,
    text: &str,
    data: AddressStepData,
) {
    let (address, geo, resolved_ip) = if text.eq_ignore_ascii_case("/skip") {
        (
            None,
            data.geo,
            data.resolved_ip.or_else(|| Some(data.current_ip.clone())),
        )
    } else {
        let Some(address) = normalize_server_address(text) else {
            server_wizards.lock().await.set_step(
                chat_id,
                ServerWizardStep::Address {
                    name: data.name,
                    current_ip: data.current_ip,
                    token: data.token,
                    geo: data.geo,
                    resolved_ip: data.resolved_ip,
                },
                Instant::now(),
            );
            let _ = bot
                .send_message(chat_id, "❌ 服务器地址不能为空；不填写请发送 /skip。")
                .await;
            return;
        };
        let metadata = detect_address_metadata(&address).await;
        (Some(address), metadata.geo, metadata.resolved_ip)
    };

    let draft = PendingServerDraft {
        chat_id,
        name: data.name,
        address,
        current_ip: data.current_ip,
        token: data.token,
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
    let address = draft.address.as_deref().unwrap_or(&draft.current_ip);
    let _ = bot
        .send_message(
            chat_id,
            format!(
                "✅ 服务器验证成功\n\n{}\n当前 IP：{}",
                format_server_display_parts(&draft.name, address, &draft.geo),
                html_escape(&draft.current_ip)
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
        name: draft.name.clone(),
        token: draft.token,
        enabled: true,
        address: draft.address.clone(),
        country: Some(draft.geo.country.clone()),
        flag: Some(draft.geo.flag.clone()),
        resolved_ip: draft
            .resolved_ip
            .clone()
            .or_else(|| Some(draft.current_ip.clone())),
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
        return;
    }

    let address = draft.address.as_deref().unwrap_or(&draft.current_ip);
    let _ = bot
        .send_message(
            chat_id,
            format!(
                "✅ 配置完成\n\n{}\nIP：{}\n\nTelegram Bot 已可以使用。",
                format_server_display_parts(&draft.name, address, &draft.geo),
                html_escape(&draft.current_ip)
            ),
        )
        .reply_markup(super::commands::start_menu_keyboard())
        .parse_mode(ParseMode::Html)
        .await;
}

pub(super) fn next_server_id(config: &AppConfig) -> String {
    let max_numeric = config
        .servers
        .iter()
        .filter_map(|server| server.id.strip_prefix("server-")?.parse::<u32>().ok())
        .max()
        .unwrap_or(0);
    format!("server-{}", max_numeric + 1)
}

#[cfg(test)]
mod tests {
    use crate::{
        bot::test_support::app_config,
        config::{AppConfig, SecretToken, ServerConfig},
    };

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

    #[test]
    fn next_server_id_uses_max_numeric_suffix_plus_one() {
        let mut config = app_config();
        config.servers[0].id = "server-1".to_string();
        config.servers[1].id = "server-3".to_string();

        assert_eq!(super::next_server_id(&config), "server-4");
    }

    #[test]
    fn next_server_id_ignores_non_matching_ids() {
        let config = AppConfig {
            servers: vec![ServerConfig {
                id: "hk-01".to_string(),
                name: "HK".to_string(),
                token: SecretToken::from_test_value("hidden-token"),
                enabled: true,
                address: None,
                country: None,
                flag: None,
                resolved_ip: None,
                timer: None,
            }],
            global_timer: None,
            tg_token: None,
            tg_chat_id: None,
            tg_pair_code: None,
            tg_pair_expires_at: None,
            migration_notice: None,
        };

        assert_eq!(super::next_server_id(&config), "server-1");
    }
}
