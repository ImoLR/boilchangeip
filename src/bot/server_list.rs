use std::sync::Arc;

use teloxide::{prelude::*, types::ParseMode};
use tokio::sync::Mutex;

use crate::{
    boil::BoilClient,
    config::{AppConfig, ServerConfig},
    server_manage::{move_server_down, move_server_up},
    timer::TimerManager,
};

use super::{
    formatting::{format_server_card, html_escape},
    server_edit::save_config_and_reload,
};

pub(super) async fn show_servers(bot: &Bot, chat_id: ChatId, config: &AppConfig) {
    if config.servers.is_empty() {
        let _ = bot
            .send_message(chat_id, "尚未添加服务器，请点击“添加服务器”。")
            .await;
        return;
    }

    let client = match BoilClient::new() {
        Ok(client) => Some(client),
        Err(error) => {
            log::warn!("服务器列表初始化 Boil API 客户端失败: {error}");
            None
        }
    };

    for server in &config.servers {
        let current_ip = match &client {
            Some(client) => match client.get_ip(&server.token).await {
                Ok(response) => response.ip.to_string(),
                Err(error) => {
                    log::warn!(
                        "服务器列表查询当前 IP 失败: server_id={}: {error}",
                        server.id
                    );
                    "查询失败".to_string()
                }
            },
            None => "查询失败".to_string(),
        };
        let _ = bot
            .send_message(chat_id, format_server_card(server, &current_ip))
            .parse_mode(ParseMode::Html)
            .await;
    }
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
    fn server_card_has_no_action_buttons_or_missing_address_text() {
        let config = app_config();
        let card = format_server_card(&config.servers[0], "203.0.113.10");

        assert!(card.contains("📡"));
        assert!(card.contains("Hong Kong 01"));
        assert!(card.contains("当前 IP：203.0.113.10"));
        assert!(!card.contains("地址未设置"));
        assert!(!card.contains("📊 状态"));
        assert!(!card.contains("🔄 更换 IP"));
        assert!(!card.contains("编辑"));
        assert!(!card.contains("删除"));
        assert!(!card.contains("hidden-token"));
        assert!(!card.contains("hk-01"));
    }
}
