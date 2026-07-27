use std::sync::Arc;
use std::time::Instant;

use teloxide::{
    prelude::*,
    types::{MessageId, ParseMode},
};
use tokio::sync::Mutex;

use crate::{
    boil::BoilClient,
    config::{save_app_config, AppConfig, SecretToken, ServerConfig},
    timer::TimerManager,
};

use super::{
    formatting::{detect_address_metadata, format_server_display_parts, html_escape},
    state::{ServerWizardStep, ServerWizardStore},
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
    config: &Arc<Mutex<AppConfig>>,
    timer: &Arc<Mutex<TimerManager>>,
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
            handle_name_step(
                bot,
                chat_id,
                config,
                timer,
                server_wizards,
                text,
                NameStepData { current_ip, token },
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
    let _ = bot
        .send_message(chat_id, "⚙️ 正在验证 Token，请稍候…")
        .await;
    let _ = bot
        .send_message(chat_id, "⚙️ 正在查询当前 IP，请稍候…")
        .await;
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
            format!("✅ Token 验证成功\n\n当前 IP：{}", html_escape(&current_ip)),
        )
        .await;
    let _ = bot
        .send_message(chat_id, "请给这台服务器设置一个显示名称：")
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
    config: &Arc<Mutex<AppConfig>>,
    timer: &Arc<Mutex<TimerManager>>,
    server_wizards: &Arc<Mutex<ServerWizardStore>>,
    text: &str,
    data: NameStepData,
) {
    let name = text.trim();
    if name.is_empty() {
        server_wizards.lock().await.set_step(
            chat_id,
            ServerWizardStep::Name {
                current_ip: data.current_ip,
                token: data.token,
            },
            Instant::now(),
        );
        let _ = bot
            .send_message(chat_id, "❌ 名称不能为空，请重新输入。")
            .await;
        return;
    }

    let metadata = detect_address_metadata(&data.current_ip).await;
    let retry_current_ip = data.current_ip.clone();
    let retry_token = data.token.clone();

    let current = config.lock().await.clone();
    let server_id = next_server_id(&current);
    let mut next = current;
    next.servers.push(ServerConfig {
        id: server_id,
        name: name.to_string(),
        token: data.token,
        enabled: true,
        address: Some(data.current_ip.clone()),
        country: Some(metadata.geo.country.clone()),
        flag: Some(metadata.geo.flag.clone()),
        resolved_ip: metadata
            .resolved_ip
            .clone()
            .or_else(|| Some(data.current_ip.clone())),
        timer: None,
    });

    if let Err(error) = save_app_config(&next) {
        server_wizards.lock().await.set_step(
            chat_id,
            ServerWizardStep::Name {
                current_ip: retry_current_ip,
                token: retry_token,
            },
            Instant::now(),
        );
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

    let _ = bot
        .send_message(
            chat_id,
            format!(
                "✅ 配置完成\n\n{}",
                format_server_display_parts(name, &data.current_ip, &metadata.geo)
            ),
        )
        .parse_mode(ParseMode::Html)
        .await;
}

struct NameStepData {
    current_ip: String,
    token: SecretToken,
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
