use std::{sync::Arc, time::Instant};

use teloxide::{
    prelude::*,
    types::{InlineKeyboardButton, InlineKeyboardMarkup, ParseMode},
};
use tokio::sync::Mutex;

use crate::{
    boil::BoilClient,
    config::{AppConfig, ResolvedSelection, ServerConfig, ServerSelection},
    core::check_ip_quality,
    reconnect::{
        reconnect_one_with_current_ip_and_progress, ReconnectPolicy, ReconnectProgress,
        ReconnectResult, ReconnectStatus,
    },
};

use super::{
    formatting::{format_server_display_parts, html_escape, server_geo_label},
    state::{ConfirmConsume, ConfirmationStore, UiPage, UiSessionStore},
    timer_ui::{record_sent_page_message, record_sent_ui_message},
};

pub(super) async fn tg_check(
    bot: &Bot,
    chat_id: ChatId,
    config: &AppConfig,
    arg: &str,
    ui_sessions: &Arc<Mutex<UiSessionStore>>,
) {
    let selection = selection_from_tg_arg(arg);
    let selected = match resolve_for_tg(bot, chat_id, config, selection, "check").await {
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

    let sent = bot
        .send_message(chat_id, "⚙️ 正在查询当前 IP，请稍候…")
        .await;
    record_sent_page_message(bot, chat_id, sent, UiPage::Check, ui_sessions).await;
    for server in selected_servers(selected) {
        let sent = bot
            .send_message(chat_id, format!("🔍 检测中: {}", html_escape(&server.name)))
            .await;
        record_sent_ui_message(chat_id, sent, ui_sessions).await;
        let response = match client.get_ip(&server.token).await {
            Ok(response) => response,
            Err(e) => {
                let _ = bot
                    .send_message(
                        chat_id,
                        format!("❌ Token 查询失败: {}", html_escape(&e.to_string())),
                    )
                    .await;
                continue;
            }
        };

        let ip = response.ip.to_string();
        let text = match check_ip_quality(&ip).await {
            Some(q) => format!(
                "📍 <b>{}</b>\nIP: <code>{}</code>\n地区: {} | ISP: {}\n类型: {} | CF 风险: {}",
                html_escape(&server.name),
                ip,
                html_escape(&q.country),
                html_escape(&q.isp),
                q.ip_type(),
                q.cf_risk()
            ),
            None => format!(
                "📍 <b>{}</b>\nIP: <code>{}</code>\nIP 质量检测失败",
                html_escape(&server.name),
                ip
            ),
        };
        let sent = bot
            .send_message(chat_id, text)
            .parse_mode(ParseMode::Html)
            .await;
        record_sent_ui_message(chat_id, sent, ui_sessions).await;
    }
}

pub(super) async fn tg_change(
    bot: &Bot,
    chat_id: ChatId,
    config: &AppConfig,
    confirmations: &Arc<Mutex<ConfirmationStore>>,
    arg: &str,
    ui_sessions: &Arc<Mutex<UiSessionStore>>,
) {
    let selection = selection_from_tg_arg(arg);
    let selected = match config.resolve_servers(selection) {
        Ok(ResolvedSelection::One(server)) => server,
        Ok(ResolvedSelection::All(_)) => {
            let _ = bot
                .send_message(chat_id, "Telegram 换 IP 不支持全部执行，请选择单台 VPS")
                .await;
            return;
        }
        Err(e) => {
            if matches!(selection, ServerSelection::Unspecified) {
                show_server_selection(bot, chat_id, config, "change").await;
            } else {
                let _ = bot
                    .send_message(chat_id, format!("❌ {}", html_escape(&e.to_string())))
                    .await;
            }
            return;
        }
    };

    show_change_confirmation(
        bot,
        chat_id,
        config,
        confirmations,
        &selected.id,
        ui_sessions,
    )
    .await;
}

pub(super) async fn show_change_confirmation(
    bot: &Bot,
    chat_id: ChatId,
    config: &AppConfig,
    confirmations: &Arc<Mutex<ConfirmationStore>>,
    server_id: &str,
    ui_sessions: &Arc<Mutex<UiSessionStore>>,
) {
    let server = match config.resolve_servers(ServerSelection::Id(server_id)) {
        Ok(ResolvedSelection::One(server)) => server,
        _ => {
            let _ = bot.send_message(chat_id, "server 不存在或已禁用").await;
            return;
        }
    };
    let nonce = confirmations
        .lock()
        .await
        .insert(&server.id, Instant::now());
    let keyboard = InlineKeyboardMarkup::new(vec![vec![
        InlineKeyboardButton::callback(
            "确认换 IP",
            format!("confirm_change:{}:{}", server.id, nonce),
        ),
        InlineKeyboardButton::callback("取消", format!("cancel_change:{}:{}", server.id, nonce)),
    ]]);
    let initial_text = format!(
        "确认更换这台服务器的 IP？\n\n{}",
        format_server_display_parts(&server.name, "⚙️ 正在查询 IP...", &server_geo_label(server))
    );
    let sent = bot
        .send_message(chat_id, initial_text)
        .reply_markup(keyboard.clone())
        .parse_mode(ParseMode::Html)
        .await;
    let message_id = sent.as_ref().ok().map(|message| message.id);
    record_sent_page_message(bot, chat_id, sent, UiPage::Change, ui_sessions).await;

    let current_ip = match BoilClient::new() {
        Ok(client) => match client.get_ip(&server.token).await {
            Ok(response) => response.ip.to_string(),
            Err(error) => {
                log::warn!(
                    "Telegram /change 确认页查询当前 IP 失败: server_id={}: {error}",
                    server.id
                );
                "查询失败".to_string()
            }
        },
        Err(error) => {
            log::warn!("Telegram /change 确认页初始化客户端失败: {error}");
            "查询失败".to_string()
        }
    };
    if let Some(message_id) = message_id {
        let _ = bot
            .edit_message_text(
                chat_id,
                message_id,
                format!(
                    "确认更换这台服务器的 IP？\n\n{}",
                    format_server_display_parts(
                        &server.name,
                        &current_ip,
                        &server_geo_label(server)
                    )
                ),
            )
            .reply_markup(keyboard)
            .parse_mode(ParseMode::Html)
            .await;
    }
}

pub(super) async fn confirm_and_change(
    bot: &Bot,
    chat_id: ChatId,
    config: &AppConfig,
    confirmations: &Arc<Mutex<ConfirmationStore>>,
    server_id: &str,
    nonce: &str,
    ui_sessions: &Arc<Mutex<UiSessionStore>>,
) {
    let consume = confirmations
        .lock()
        .await
        .consume(server_id, nonce, Instant::now());
    match consume {
        ConfirmConsume::Accepted => {}
        ConfirmConsume::Expired | ConfirmConsume::Missing => {
            let _ = bot
                .send_message(chat_id, "确认已过期，请重新发送 /change")
                .await;
            return;
        }
        ConfirmConsume::AlreadyUsed => {
            let _ = bot
                .send_message(chat_id, "该确认已使用，请勿重复点击")
                .await;
            return;
        }
        ConfirmConsume::Mismatch => {
            let _ = bot
                .send_message(chat_id, "确认信息不匹配，请重新发送 /change")
                .await;
            return;
        }
    }

    let server = match config.resolve_servers(ServerSelection::Id(server_id)) {
        Ok(ResolvedSelection::One(server)) => server,
        _ => {
            let _ = bot.send_message(chat_id, "server 不存在或已禁用").await;
            return;
        }
    };

    let sent = bot.send_message(chat_id, "⏳ 开始换 IP，请稍候...").await;
    record_sent_page_message(bot, chat_id, sent, UiPage::Change, ui_sessions).await;
    let client = match BoilClient::new() {
        Ok(client) => client,
        Err(e) => {
            let _ = bot
                .send_message(chat_id, format!("❌ 初始化失败: {e}"))
                .await;
            return;
        }
    };
    let sent = bot
        .send_message(chat_id, "⚙️ 正在查询当前 IP，请稍候…")
        .await;
    record_sent_ui_message(chat_id, sent, ui_sessions).await;
    let progress_bot = bot.clone();
    let progress_ui_sessions = Arc::clone(ui_sessions);
    let result = reconnect_one_with_current_ip_and_progress(
        &client,
        server,
        &ReconnectPolicy::default(),
        None,
        move |progress| {
            let bot = progress_bot.clone();
            let ui_sessions = Arc::clone(&progress_ui_sessions);
            async move {
                match progress {
                    ReconnectProgress::VerifyingNewIp { .. } => {
                        let sent = bot.send_message(chat_id, "⚙️ 正在查询新 IP，请稍候…").await;
                        record_sent_ui_message(chat_id, sent, &ui_sessions).await;
                    }
                }
            }
        },
    )
    .await;
    send_reconnect_result(bot, chat_id, &result, ui_sessions).await;
}

pub(super) async fn resolve_for_tg<'a>(
    bot: &Bot,
    chat_id: ChatId,
    config: &'a AppConfig,
    selection: ServerSelection<'_>,
    action: &str,
) -> Option<ResolvedSelection<'a>> {
    match config.resolve_servers(selection) {
        Ok(selected) => Some(selected),
        Err(e) if matches!(selection, ServerSelection::Unspecified) => {
            show_server_selection(bot, chat_id, config, action).await;
            log::debug!("Telegram 需要选择 VPS: {e}");
            None
        }
        Err(e) => {
            let _ = bot
                .send_message(chat_id, format!("❌ {}", html_escape(&e.to_string())))
                .await;
            None
        }
    }
}

async fn show_server_selection(bot: &Bot, chat_id: ChatId, config: &AppConfig, action: &str) {
    let buttons: Vec<Vec<InlineKeyboardButton>> = config
        .servers
        .iter()
        .filter(|server| server.enabled)
        .map(|server| {
            vec![InlineKeyboardButton::callback(
                server.name.clone(),
                format!("select_{action}:{}", server.id),
            )]
        })
        .collect();

    if buttons.is_empty() {
        let _ = bot.send_message(chat_id, "没有已启用的 VPS").await;
        return;
    }

    let _ = bot
        .send_message(chat_id, "请选择 VPS：")
        .reply_markup(InlineKeyboardMarkup::new(buttons))
        .await;
}

async fn send_reconnect_result(
    bot: &Bot,
    chat_id: ChatId,
    result: &ReconnectResult,
    ui_sessions: &Arc<Mutex<UiSessionStore>>,
) {
    if result.status == ReconnectStatus::RateLimited {
        let mut lines = vec!["⏳ 换 IP 频率限制中".to_string(), String::new()];
        lines.push(format!("📡 <b>{}</b>", html_escape(&result.server_name)));
        if let Some(message) = &result.message {
            let message = message
                .lines()
                .filter(|line| !line.trim().starts_with('⏳'))
                .collect::<Vec<_>>()
                .join("\n");
            if !message.trim().is_empty() {
                lines.push(String::new());
                lines.push(html_escape(message.trim()));
            }
        }
        let sent = bot
            .send_message(chat_id, lines.join("\n"))
            .parse_mode(ParseMode::Html)
            .await;
        record_sent_ui_message(chat_id, sent, ui_sessions).await;
        return;
    }

    let mut lines = vec![
        format!("📡 <b>{}</b>", html_escape(&result.server_name)),
        format!("状态: {:?}", result.status),
    ];
    if let Some(old_ip) = result.old_ip {
        lines.push(format!("旧 IP: <code>{old_ip}</code>"));
    }
    if let Some(new_ip) = result.new_ip {
        lines.push(format!("新 IP: <code>{new_ip}</code>"));
    }
    if let Some(uses_left) = result.uses_left {
        lines.push(format!("剩余次数: {uses_left}"));
    }
    if let Some(message) = &result.message {
        lines.push(format!("信息: {}", html_escape(message)));
    }

    let sent = bot
        .send_message(chat_id, lines.join("\n"))
        .parse_mode(ParseMode::Html)
        .await;
    record_sent_ui_message(chat_id, sent, ui_sessions).await;
}

pub(super) fn selection_from_tg_arg(arg: &str) -> ServerSelection<'_> {
    let trimmed = arg.trim();
    if trimmed.is_empty() {
        ServerSelection::Unspecified
    } else {
        ServerSelection::Id(trimmed)
    }
}

pub(super) fn selected_servers(selected: ResolvedSelection<'_>) -> Vec<&ServerConfig> {
    match selected {
        ResolvedSelection::One(server) => vec![server],
        ResolvedSelection::All(servers) => servers,
    }
}
