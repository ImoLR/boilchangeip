use std::time::{Duration, SystemTime, UNIX_EPOCH};

use teloxide::{
    prelude::*,
    types::{InlineKeyboardButton, InlineKeyboardMarkup, InputFile, ParseMode},
};

use crate::{
    boil::BoilClient,
    config::{AppConfig, ServerConfig},
    status_card::{CountdownState, StatusCardData, StatusCardState, TempStatusCard},
    timer::parse_hhmm,
};

use super::{
    change::{resolve_for_tg, selected_servers, selection_from_tg_arg},
    formatting::{server_geo_label, short_safe_error},
};

pub(super) async fn tg_status(bot: &Bot, chat_id: ChatId, config: &AppConfig, arg: &str) {
    let selection = selection_from_tg_arg(arg);
    let selected = match resolve_for_tg(bot, chat_id, config, selection, "status").await {
        Some(selected) => selected,
        None => return,
    };

    let client = match BoilClient::new() {
        Ok(client) => client,
        Err(e) => {
            let _ = bot
                .send_message(chat_id, format!("❌ 初始化失败: {e}"))
                .await;
            return;
        }
    };

    for server in selected_servers(selected) {
        let (status, detail, current_ip) = match client.get_ip(&server.token).await {
            Ok(response) => (StatusCardState::Normal, None, Some(response.ip.to_string())),
            Err(e) => (
                StatusCardState::VerificationFailed,
                Some(short_safe_error(&e.to_string())),
                None,
            ),
        };
        let data = status_card_data(config, server, status, current_ip.as_deref(), detail);
        send_status_card_or_text(bot, chat_id, server, data).await;
    }
}

async fn send_status_card_or_text(
    bot: &Bot,
    chat_id: ChatId,
    server: &ServerConfig,
    data: StatusCardData,
) {
    let keyboard = status_card_keyboard(server);
    match TempStatusCard::render(&data) {
        Ok(card) => {
            let result = bot
                .send_photo(chat_id, InputFile::file(card.path().to_path_buf()))
                .reply_markup(keyboard.clone())
                .await;
            if result.is_ok() {
                drop(card);
                return;
            }
            drop(card);
            let _ = bot
                .send_message(chat_id, crate::status_card::fallback_text(&data))
                .reply_markup(keyboard)
                .parse_mode(ParseMode::Html)
                .await;
        }
        Err(error) => {
            log::warn!("状态卡片生成失败，回退为文本: {error}");
            let _ = bot
                .send_message(chat_id, crate::status_card::fallback_text(&data))
                .reply_markup(keyboard)
                .parse_mode(ParseMode::Html)
                .await;
        }
    }
}

pub(super) fn status_card_data(
    config: &AppConfig,
    server: &ServerConfig,
    status: StatusCardState,
    current_ip: Option<&str>,
    detail: Option<String>,
) -> StatusCardData {
    StatusCardData {
        server_name: server.name.clone(),
        region: server_geo_label(server).display(),
        address: server
            .address
            .as_deref()
            .or(current_ip)
            .unwrap_or("地址未设置")
            .to_string(),
        status,
        countdown: status_countdown(config, server),
        detail,
    }
}

pub(super) fn status_countdown(config: &AppConfig, server: &ServerConfig) -> CountdownState {
    if let Some(timer) = &server.timer {
        if !timer.enabled {
            return CountdownState::Paused;
        }
        if let Some(hhmm) = timer.cron.as_deref().and_then(crate::timer::cron_to_hhmm) {
            return next_daily_countdown(&hhmm).unwrap_or(CountdownState::NotAvailable);
        }
    }

    if let Some(timer) = &config.global_timer {
        if timer.enabled {
            if let Some(hhmm) = timer.cron.as_deref().and_then(crate::timer::cron_to_hhmm) {
                return next_daily_countdown(&hhmm).unwrap_or(CountdownState::NotAvailable);
            }
        } else if timer.cron.is_some() {
            return CountdownState::Paused;
        }
    }

    CountdownState::NotAvailable
}

fn next_daily_countdown(hhmm: &str) -> Option<CountdownState> {
    let (hour, minute) = parse_hhmm(hhmm).ok()?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    let shanghai_now = now + 8 * 3_600;
    let seconds_today = shanghai_now % 86_400;
    let target_today = u64::from(hour) * 3_600 + u64::from(minute) * 60;
    let seconds_until = if target_today > seconds_today {
        target_today - seconds_today
    } else {
        86_400 - seconds_today + target_today
    };
    Some(CountdownState::Duration(Duration::from_secs(seconds_until)))
}

pub(super) fn status_card_keyboard(server: &ServerConfig) -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![
        vec![
            InlineKeyboardButton::callback("🔄 更换 IP", format!("select_change:{}", server.id)),
            InlineKeyboardButton::callback(
                "⏰ 定时任务",
                format!("timer_edit_target:server:{}", server.id),
            ),
        ],
        vec![
            InlineKeyboardButton::callback("✏️ 编辑服务器", format!("server_edit:{}", server.id)),
            InlineKeyboardButton::callback("🗑 删除服务器", format!("server_delete:{}", server.id)),
        ],
        vec![InlineKeyboardButton::callback(
            "⬅️ 返回服务器列表",
            "menu:servers",
        )],
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{bot::test_support::app_config, config::ServerTimerConfig};

    #[test]
    fn status_card_data_uses_public_fields_only() {
        let config = app_config();
        let data = status_card_data(
            &config,
            &config.servers[0],
            StatusCardState::Normal,
            Some("42.0.0.1"),
            None,
        );
        let debug = format!("{data:?}");

        assert!(data.server_name.contains("Hong Kong 01"));
        assert!(data.region.contains("中国香港"));
        assert_eq!(data.address, "203.0.113.10");
        assert!(!debug.contains("hk-01"));
        assert!(!debug.contains("hidden-token"));
    }

    #[test]
    fn status_countdown_handles_missing_and_paused_timers() {
        let mut config = app_config();
        config.global_timer = None;
        config.servers[0].timer = None;
        assert_eq!(
            status_countdown(&config, &config.servers[0]),
            CountdownState::NotAvailable
        );

        config.servers[0].timer = Some(ServerTimerConfig {
            enabled: false,
            cron: Some("30 3 * * *".to_string()),
        });
        assert_eq!(
            status_countdown(&config, &config.servers[0]),
            CountdownState::Paused
        );
    }

    #[test]
    fn status_card_keyboard_keeps_internal_id_out_of_labels() {
        let config = app_config();
        let keyboard = status_card_keyboard(&config.servers[0]);
        let debug = format!("{keyboard:?}");

        assert!(debug.contains("🔄 更换 IP"));
        assert!(debug.contains("⏰ 定时任务"));
        assert!(debug.contains("编辑服务器"));
        assert!(debug.contains("🗑 删除服务器"));
        assert!(debug.contains("返回服务器列表"));
        assert!(debug.contains("select_change:hk-01"));
        assert!(!debug.contains("hidden-token"));
    }
}
