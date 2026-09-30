use std::{sync::Arc, time::Instant};

use teloxide::{
    prelude::*,
    types::{InlineKeyboardButton, InlineKeyboardMarkup, ParseMode},
};
use tokio::sync::Mutex;

use crate::{
    config::{AppConfig, ResolvedSelection, ServerConfig, ServerSelection},
    core::check_ip_quality,
    provider::ProviderClients,
    reconnect::{
        reconnect_provider_one_with_current_ip_and_progress, ReconnectPolicy, ReconnectProgress,
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

    let client = match ProviderClients::new() {
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
        let ip = match client.get_ip(server).await {
            Ok(ip) => ip,
            Err(e) => {
                let _ = bot
                    .send_message(
                        chat_id,
                        format!("❌ 当前 IP 查询失败: {}", html_escape(&e.to_string())),
                    )
                    .await;
                continue;
            }
        };

        let ip = ip.to_string();
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

    let current_ip = match ProviderClients::new() {
        Ok(client) => match client.get_ip(server).await {
            Ok(ip) => ip.to_string(),
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
    let client = match ProviderClients::new() {
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
    let progress_server_name = server.name.clone();
    let result = reconnect_provider_one_with_current_ip_and_progress(
        &client,
        server,
        &ReconnectPolicy::default(),
        None,
        move |progress| {
            let bot = progress_bot.clone();
            let ui_sessions = Arc::clone(&progress_ui_sessions);
            let server_name = progress_server_name.clone();
            async move {
                match progress {
                    ReconnectProgress::VerifyingNewIp { .. } => {
                        let sent = bot.send_message(chat_id, "⚙️ 正在查询新 IP，请稍候…").await;
                        record_sent_ui_message(chat_id, sent, &ui_sessions).await;
                    }
                    ReconnectProgress::ProviderConfirmedRedial { old_ip, new_ip } => {
                        let message = format!(
                            "✅ 拨号成功\n\n📡 <b>{}</b>\n\n旧 IP：<code>{old_ip}</code>\n新 IP：<code>{new_ip}</code>\n\n⏳ 正在等待网络恢复…",
                            html_escape(&server_name)
                        );
                        let sent = bot
                            .send_message(chat_id, message)
                            .parse_mode(ParseMode::Html)
                            .await;
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
    if result.dial_attempts > 0 {
        let sent = bot
            .send_message(chat_id, format_dial_reconnect_result(result))
            .parse_mode(ParseMode::Html)
            .await;
        record_sent_ui_message(chat_id, sent, ui_sessions).await;
        return;
    }

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

fn format_dial_reconnect_result(result: &ReconnectResult) -> String {
    let (icon, status_text) = match result.status {
        ReconnectStatus::Success => ("✅", "换 IP 成功"),
        ReconnectStatus::ChangeAcceptedButUnconfirmed => {
            ("⚠️", "已完成拨号，但暂时无法确认网络恢复")
        }
        _ => ("❌", "换 IP 失败"),
    };
    let mut lines = vec![format!(
        "{icon} <b>{}</b> {status_text}",
        html_escape(&result.server_name),
    )];
    lines.push(String::new());
    if let Some(old_ip) = result.old_ip {
        lines.push(format!("旧 IP：<code>{old_ip}</code>"));
    }
    lines.push(match result.new_ip {
        Some(ip) => format!("新 IP：<code>{ip}</code>"),
        None => "新 IP：无法确认".to_string(),
    });
    lines.push(format!("本次拨号尝试：{} 次", result.dial_attempts));
    if let Some(count) = result.provider_dial_count {
        lines.push(format!("接口拨号计数：{count}"));
    }
    if result.status == ReconnectStatus::ChangeAcceptedButUnconfirmed {
        lines.push(String::new());
        lines.push("请稍后查看状态".to_string());
    } else if result.status == ReconnectStatus::ProviderFailed {
        lines.push(String::new());
        lines.push("⚠️ 已停止自动重试，请管理员处理".to_string());
    }
    lines.join("\n")
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

#[cfg(test)]
mod tests {
    use super::*;

    fn fachost_result(status: ReconnectStatus) -> ReconnectResult {
        ReconnectResult {
            server_id: "seednet-1".to_string(),
            server_name: "Seednet VPS".to_string(),
            old_ip: Some("8.8.8.8".parse().unwrap()),
            new_ip: Some("8.8.4.4".parse().unwrap()),
            changed: status == ReconnectStatus::Success,
            uses_left: None,
            next_allowed_at: None,
            status,
            message: None,
            poll_attempts: 24,
            dial_attempts: 1,
            provider_dial_count: Some(7),
        }
    }

    #[test]
    fn fachost_result_distinguishes_attempts_from_provider_counter() {
        let text = format_dial_reconnect_result(&fachost_result(ReconnectStatus::Success));
        assert!(text.contains("本次拨号尝试：1 次"));
        assert!(text.contains("接口拨号计数：7"));
        assert!(!text.contains("拨号次数："));
        assert!(!text.contains("今日已用"));
        assert!(!text.contains("/ 5"));
    }

    #[test]
    fn fachost_confirmed_but_unreachable_is_not_reported_as_failure() {
        let text = format_dial_reconnect_result(&fachost_result(
            ReconnectStatus::ChangeAcceptedButUnconfirmed,
        ));
        assert!(text.contains("⚠️ <b>Seednet VPS</b> 已完成拨号，但暂时无法确认网络恢复"));
        assert!(text.contains("请稍后查看状态"));
        assert!(!text.contains("换 IP 失败"));
        assert!(!text.contains("已停止自动重试"));
    }
}
