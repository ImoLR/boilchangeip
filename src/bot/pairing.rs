use std::{
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use teloxide::prelude::*;
use tokio::sync::Mutex;

use crate::config::{save_app_config, AppConfig};

use super::{
    commands::{send_start_menu, sync_bot_menu},
    formatting::html_escape,
};

pub(super) async fn handle_pair_command(
    bot: &Bot,
    chat_id: ChatId,
    config: &Arc<Mutex<AppConfig>>,
    code: &str,
) {
    let chat_id_str = chat_id.to_string();
    let result = {
        let mut guard = config.lock().await;
        apply_pairing(
            &mut guard,
            &chat_id_str,
            code,
            current_unix_timestamp(),
            save_app_config,
        )
    };

    match result {
        PairingApplyResult::Paired => {
            sync_bot_menu(bot).await;
            let _ = bot
                .send_message(chat_id, "✅ 配对成功，已启用 Telegram 菜单。")
                .await;
            let _ = send_start_menu(bot, chat_id).await;
        }
        PairingApplyResult::InvalidOrExpired => {
            let _ = bot.send_message(chat_id, "配对码无效或已过期").await;
        }
        PairingApplyResult::AlreadyBound => {
            let _ = bot.send_message(chat_id, "拒绝访问").await;
        }
        PairingApplyResult::SaveFailed(error) => {
            let _ = bot
                .send_message(
                    chat_id,
                    format!("❌ 配对失败，无法保存配置: {}", html_escape(&error)),
                )
                .await;
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum PairingDecision {
    Paired(AppConfig),
    InvalidOrExpired(Option<AppConfig>),
    AlreadyBound,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum PairingApplyResult {
    Paired,
    InvalidOrExpired,
    AlreadyBound,
    SaveFailed(String),
}

pub(super) fn resolve_pairing(
    current: &AppConfig,
    chat_id: &str,
    code: &str,
    now: u64,
) -> PairingDecision {
    if current.tg_chat_id.is_some() {
        return PairingDecision::AlreadyBound;
    }

    let Some(stored_code) = current.tg_pair_code.as_ref() else {
        return PairingDecision::InvalidOrExpired(None);
    };
    let Some(expires_at) = current.tg_pair_expires_at else {
        return PairingDecision::InvalidOrExpired(None);
    };

    if expires_at <= now {
        let mut next = current.clone();
        next.tg_pair_code = None;
        next.tg_pair_expires_at = None;
        return PairingDecision::InvalidOrExpired(Some(next));
    }

    if stored_code.expose_secret() != code.trim().to_ascii_uppercase() {
        return PairingDecision::InvalidOrExpired(None);
    }

    let mut next = current.clone();
    next.tg_chat_id = Some(chat_id.to_string());
    next.tg_pair_code = None;
    next.tg_pair_expires_at = None;
    PairingDecision::Paired(next)
}

pub(super) fn apply_pairing(
    current: &mut AppConfig,
    chat_id: &str,
    code: &str,
    now: u64,
    save: impl FnOnce(&AppConfig) -> anyhow::Result<()>,
) -> PairingApplyResult {
    match resolve_pairing(current, chat_id, code, now) {
        PairingDecision::Paired(next) => match save(&next) {
            Ok(()) => {
                *current = next;
                PairingApplyResult::Paired
            }
            Err(error) => PairingApplyResult::SaveFailed(error.to_string()),
        },
        PairingDecision::InvalidOrExpired(Some(next)) => {
            if save(&next).is_ok() {
                *current = next;
            }
            PairingApplyResult::InvalidOrExpired
        }
        PairingDecision::InvalidOrExpired(None) => PairingApplyResult::InvalidOrExpired,
        PairingDecision::AlreadyBound => PairingApplyResult::AlreadyBound,
    }
}

fn current_unix_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

pub(super) fn is_authorized_tg_id(config: &AppConfig, id: &str) -> bool {
    config.tg_chat_id.as_deref() == Some(id)
}

pub(super) fn is_authorized_callback_chat(config: &AppConfig, chat_id: ChatId) -> bool {
    is_authorized_tg_id(config, &chat_id.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bot::test_support::{app_config, pairable_config};

    #[test]
    fn start_without_pairing_does_not_authorize_chat() {
        let config = pairable_config();

        assert!(!is_authorized_tg_id(&config, "99999"));
        assert!(config.tg_chat_id.is_none());
    }

    #[test]
    fn correct_pairing_code_binds_chat_and_consumes_code() {
        let config = pairable_config();

        let PairingDecision::Paired(next) = resolve_pairing(&config, "99999", "TEST-CODE", 999)
        else {
            panic!("expected pairing success");
        };

        assert_eq!(next.tg_chat_id.as_deref(), Some("99999"));
        assert!(next.tg_pair_code.is_none());
        assert!(next.tg_pair_expires_at.is_none());
    }

    #[test]
    fn wrong_pairing_code_cannot_bind_chat() {
        let config = pairable_config();

        assert_eq!(
            resolve_pairing(&config, "99999", "WRNG-0000", 999),
            PairingDecision::InvalidOrExpired(None)
        );
        assert!(config.tg_chat_id.is_none());
    }

    #[test]
    fn expired_pairing_code_cannot_bind_and_is_consumed() {
        let config = pairable_config();

        let PairingDecision::InvalidOrExpired(Some(next)) =
            resolve_pairing(&config, "99999", "TEST-CODE", 1_000)
        else {
            panic!("expected expired pairing code");
        };

        assert!(next.tg_chat_id.is_none());
        assert!(next.tg_pair_code.is_none());
        assert!(next.tg_pair_expires_at.is_none());
    }

    #[test]
    fn pairing_code_can_only_be_used_once() {
        let config = pairable_config();
        let PairingDecision::Paired(next) = resolve_pairing(&config, "99999", "TEST-CODE", 999)
        else {
            panic!("expected first pairing success");
        };

        assert_eq!(
            resolve_pairing(&next, "88888", "TEST-CODE", 999),
            PairingDecision::AlreadyBound
        );
    }

    #[test]
    fn bound_chat_cannot_be_overwritten_by_another_chat() {
        let mut config = pairable_config();
        config.tg_chat_id = Some("99999".to_string());

        assert_eq!(
            resolve_pairing(&config, "88888", "TEST-CODE", 999),
            PairingDecision::AlreadyBound
        );
        assert_eq!(config.tg_chat_id.as_deref(), Some("99999"));
    }

    #[test]
    fn concurrent_pairing_attempts_only_allow_one_success() {
        let mut config = pairable_config();

        assert_eq!(
            apply_pairing(&mut config, "11111", "TEST-CODE", 999, |_| Ok(())),
            PairingApplyResult::Paired
        );
        assert_eq!(
            apply_pairing(&mut config, "22222", "TEST-CODE", 999, |_| Ok(())),
            PairingApplyResult::AlreadyBound
        );
        assert_eq!(config.tg_chat_id.as_deref(), Some("11111"));
    }

    #[test]
    fn pairing_save_failure_does_not_update_authorized_chat() {
        let mut config = pairable_config();

        assert_eq!(
            apply_pairing(&mut config, "11111", "TEST-CODE", 999, |_| {
                anyhow::bail!("mock save failure")
            }),
            PairingApplyResult::SaveFailed("mock save failure".to_string())
        );
        assert!(config.tg_chat_id.is_none());
        assert!(config.tg_pair_code.is_some());
        assert_eq!(config.tg_pair_expires_at, Some(1_000));
    }

    #[test]
    fn unauthorized_callback_chat_is_rejected_by_chat_id() {
        let config = app_config();

        assert!(is_authorized_callback_chat(&config, ChatId(12345)));
        assert!(!is_authorized_callback_chat(&config, ChatId(54321)));
    }
}
