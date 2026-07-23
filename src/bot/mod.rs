use std::sync::Arc;

use teloxide::prelude::*;
use tokio::sync::Mutex;

use crate::{config::AppConfig, timer::TimerManager};

mod callbacks;
mod change;
mod commands;
mod formatting;
mod pairing;
mod server_delete;
mod server_edit;
mod server_list;
mod server_wizard;
mod state;
mod status;
mod timer_ui;

use callbacks::handle_callback;
use commands::{handle_command, handle_message, sync_bot_menu, Command};
use state::{BotShared, ConfirmationStore, ServerEditStore, ServerWizardStore, TimerInputStore};

pub async fn run(config: AppConfig) -> anyhow::Result<()> {
    let token = config
        .tg_token
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("未配置 TG_TOKEN，请在 config.env 中配置"))?;

    let bot = Bot::new(token);
    sync_bot_menu(&bot).await;

    let timer_config = Arc::new(config.clone());
    let shared = BotShared {
        config: Arc::new(Mutex::new(config)),
        timer: Arc::new(Mutex::new(TimerManager::new(timer_config).await?)),
        confirmations: Arc::new(Mutex::new(ConfirmationStore::default())),
        timer_inputs: Arc::new(Mutex::new(TimerInputStore::default())),
        server_wizards: Arc::new(Mutex::new(ServerWizardStore::default())),
        server_edits: Arc::new(Mutex::new(ServerEditStore::default())),
    };

    let handler = dptree::entry()
        .branch(
            Update::filter_message()
                .filter_command::<Command>()
                .endpoint(handle_command),
        )
        .branch(Update::filter_message().endpoint(handle_message))
        .branch(Update::filter_callback_query().endpoint(handle_callback));

    Dispatcher::builder(bot, handler)
        .dependencies(dptree::deps![shared])
        .enable_ctrlc_handler()
        .build()
        .dispatch()
        .await;

    Ok(())
}

#[cfg(test)]
pub(super) mod test_support {
    use crate::config::{AppConfig, PairingCode, SecretToken, ServerConfig, ServerTimerConfig};

    pub(super) fn app_config() -> AppConfig {
        AppConfig {
            servers: vec![
                ServerConfig {
                    id: "hk-01".to_string(),
                    name: "Hong Kong 01".to_string(),
                    token: SecretToken::from_test_value("hidden-token-a"),
                    enabled: true,
                    address: Some("203.0.113.10".to_string()),
                    country: Some("中国香港".to_string()),
                    flag: Some("🇭🇰".to_string()),
                    resolved_ip: None,
                    timer: Some(ServerTimerConfig {
                        enabled: true,
                        cron: Some("30 3 * * *".to_string()),
                    }),
                },
                ServerConfig {
                    id: "jp_02".to_string(),
                    name: "Japan 02".to_string(),
                    token: SecretToken::from_test_value("hidden-token-b"),
                    enabled: true,
                    address: Some("jp.example.com".to_string()),
                    country: Some("日本".to_string()),
                    flag: Some("🇯🇵".to_string()),
                    resolved_ip: None,
                    timer: None,
                },
            ],
            global_timer: Some(ServerTimerConfig {
                enabled: true,
                cron: Some("45 4 * * *".to_string()),
            }),
            tg_token: None,
            tg_chat_id: Some("12345".to_string()),
            tg_pair_code: None,
            tg_pair_expires_at: None,
            migration_notice: None,
        }
    }

    pub(super) fn pairable_config() -> AppConfig {
        let mut config = app_config();
        config.tg_chat_id = None;
        config.tg_pair_code = Some(PairingCode::new("TEST-CODE".to_string()).unwrap());
        config.tg_pair_expires_at = Some(1_000);
        config
    }
}
