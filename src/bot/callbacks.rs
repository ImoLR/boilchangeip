use std::time::Instant;

use teloxide::prelude::*;

use crate::timer::{TimerTarget, TimerUpdate};

use super::{
    change::{confirm_and_change, show_change_confirmation, tg_change, tg_check},
    commands::send_help,
    pairing::is_authorized_callback_chat,
    server_delete::{delete_server_from_config, show_server_delete_confirmation},
    server_edit::{revalidate_server, show_server_edit_menu},
    server_list::{move_server, show_servers},
    server_wizard::{confirm_add_server, start_add_server_wizard},
    state::{BotShared, ServerEditMode, TimerInputMode},
    status::tg_status,
    timer_ui::{
        apply_timer_change, show_timer_close_targets, show_timer_edit_targets, show_timer_panel,
    },
};

#[derive(Debug, PartialEq, Eq)]
pub(super) enum CallbackAction<'a> {
    MenuAddServer,
    MenuServers,
    MenuStatus,
    MenuChange,
    MenuTimer,
    MenuHelp,
    ConfirmAddServer(&'a str),
    RetryAddServer(&'a str),
    CancelAddServer(&'a str),
    SelectStatus(&'a str),
    SelectCheck(&'a str),
    SelectChange(&'a str),
    ConfirmChange { server_id: &'a str, nonce: &'a str },
    CancelChange { server_id: &'a str, nonce: &'a str },
    TimerNew,
    TimerEdit,
    TimerClose,
    TimerRefresh,
    TimerCreateAll { hhmm: &'a str },
    TimerCreateServer { server_id: &'a str, hhmm: &'a str },
    TimerEditTargetAll,
    TimerEditTargetServer(&'a str),
    TimerCloseAll,
    TimerCloseServer(&'a str),
    ServerEdit(&'a str),
    ServerEditName(&'a str),
    ServerEditAddress(&'a str),
    ServerEditToken(&'a str),
    ServerRevalidate(&'a str),
    ServerDelete(&'a str),
    ServerDeleteConfirm(&'a str),
    ServerDeleteCancel,
    ServerMoveUp(&'a str),
    ServerMoveDown(&'a str),
    LegacyChange,
    Unknown,
}

pub(super) async fn handle_callback(
    bot: Bot,
    q: CallbackQuery,
    shared: BotShared,
) -> ResponseResult<()> {
    let chat_id = match &q.message {
        Some(msg) => msg.chat.id,
        None => return Ok(()),
    };
    if !is_authorized_callback_chat(&*shared.config.lock().await, chat_id) {
        bot.answer_callback_query(&q.id).await?;
        return Ok(());
    }
    bot.answer_callback_query(&q.id).await?;

    let Some(data) = q.data.as_deref() else {
        return Ok(());
    };

    match parse_callback(data) {
        CallbackAction::MenuAddServer => {
            start_add_server_wizard(&bot, chat_id, &shared.server_wizards).await;
        }
        CallbackAction::MenuServers => {
            shared.server_edits.lock().await.cancel(chat_id);
            let config_snapshot = shared.config.lock().await.clone();
            show_servers(&bot, chat_id, &config_snapshot).await;
        }
        CallbackAction::MenuStatus => {
            let config_snapshot = shared.config.lock().await.clone();
            tg_status(&bot, chat_id, &config_snapshot, "").await;
        }
        CallbackAction::MenuChange => {
            let config_snapshot = shared.config.lock().await.clone();
            tg_change(&bot, chat_id, &config_snapshot, &shared.confirmations, "").await;
        }
        CallbackAction::MenuTimer => {
            show_timer_panel(&bot, chat_id, &shared.timer).await;
        }
        CallbackAction::MenuHelp => {
            let _ = send_help(&bot, chat_id).await;
        }
        CallbackAction::ConfirmAddServer(nonce) => {
            confirm_add_server(
                &bot,
                chat_id,
                &shared.config,
                &shared.timer,
                &shared.server_wizards,
                nonce,
            )
            .await;
        }
        CallbackAction::RetryAddServer(nonce) => {
            shared.server_wizards.lock().await.cancel_draft(nonce);
            start_add_server_wizard(&bot, chat_id, &shared.server_wizards).await;
        }
        CallbackAction::CancelAddServer(nonce) => {
            shared.server_wizards.lock().await.cancel_draft(nonce);
            let _ = bot.send_message(chat_id, "已取消添加服务器").await;
        }
        CallbackAction::SelectStatus(server_id) => {
            let config_snapshot = shared.config.lock().await.clone();
            tg_status(&bot, chat_id, &config_snapshot, server_id).await;
        }
        CallbackAction::SelectCheck(server_id) => {
            let config_snapshot = shared.config.lock().await.clone();
            tg_check(&bot, chat_id, &config_snapshot, server_id).await;
        }
        CallbackAction::SelectChange(server_id) => {
            let config_snapshot = shared.config.lock().await.clone();
            show_change_confirmation(
                &bot,
                chat_id,
                &config_snapshot,
                &shared.confirmations,
                server_id,
            )
            .await;
        }
        CallbackAction::ConfirmChange { server_id, nonce } => {
            let config_snapshot = shared.config.lock().await.clone();
            confirm_and_change(
                &bot,
                chat_id,
                &config_snapshot,
                &shared.confirmations,
                server_id,
                nonce,
            )
            .await;
        }
        CallbackAction::CancelChange { server_id, nonce } => {
            let _ = shared
                .confirmations
                .lock()
                .await
                .consume(server_id, nonce, Instant::now());
            let _ = bot.send_message(chat_id, "已取消换 IP").await;
        }
        CallbackAction::TimerNew => {
            shared
                .timer_inputs
                .lock()
                .await
                .set(chat_id, TimerInputMode::New, Instant::now());
            let _ = bot
                .send_message(chat_id, "请输入每天执行时间（HH:MM），例如 03:30")
                .await;
        }
        CallbackAction::TimerEdit => {
            show_timer_edit_targets(&bot, chat_id, &shared.timer).await;
        }
        CallbackAction::TimerClose => {
            show_timer_close_targets(&bot, chat_id, &shared.timer).await;
        }
        CallbackAction::TimerRefresh => {
            show_timer_panel(&bot, chat_id, &shared.timer).await;
        }
        CallbackAction::TimerCreateAll { hhmm } => {
            apply_timer_change(
                &bot,
                chat_id,
                &shared.timer,
                TimerUpdate::Enable {
                    target: TimerTarget::AllEnabled,
                    hhmm: hhmm.to_string(),
                },
            )
            .await;
        }
        CallbackAction::TimerCreateServer { server_id, hhmm } => {
            apply_timer_change(
                &bot,
                chat_id,
                &shared.timer,
                TimerUpdate::Enable {
                    target: TimerTarget::Server(server_id.to_string()),
                    hhmm: hhmm.to_string(),
                },
            )
            .await;
        }
        CallbackAction::TimerEditTargetAll => {
            shared.timer_inputs.lock().await.set(
                chat_id,
                TimerInputMode::Edit(TimerTarget::AllEnabled),
                Instant::now(),
            );
            let _ = bot
                .send_message(chat_id, "请输入新的每天执行时间（HH:MM），例如 03:30")
                .await;
        }
        CallbackAction::TimerEditTargetServer(server_id) => {
            shared.timer_inputs.lock().await.set(
                chat_id,
                TimerInputMode::Edit(TimerTarget::Server(server_id.to_string())),
                Instant::now(),
            );
            let _ = bot
                .send_message(chat_id, "请输入新的每天执行时间（HH:MM），例如 03:30")
                .await;
        }
        CallbackAction::TimerCloseAll => {
            apply_timer_change(
                &bot,
                chat_id,
                &shared.timer,
                TimerUpdate::Disable {
                    target: TimerTarget::AllEnabled,
                },
            )
            .await;
        }
        CallbackAction::TimerCloseServer(server_id) => {
            apply_timer_change(
                &bot,
                chat_id,
                &shared.timer,
                TimerUpdate::Disable {
                    target: TimerTarget::Server(server_id.to_string()),
                },
            )
            .await;
        }
        CallbackAction::ServerEdit(server_id) => {
            let config_snapshot = shared.config.lock().await.clone();
            show_server_edit_menu(&bot, chat_id, &config_snapshot, server_id).await;
        }
        CallbackAction::ServerEditName(server_id) => {
            shared.server_edits.lock().await.set(
                chat_id,
                ServerEditMode::Name {
                    server_id: server_id.to_string(),
                },
                Instant::now(),
            );
            let _ = bot.send_message(chat_id, "请输入新的服务器名称：").await;
        }
        CallbackAction::ServerEditAddress(server_id) => {
            shared.server_edits.lock().await.set(
                chat_id,
                ServerEditMode::Address {
                    server_id: server_id.to_string(),
                },
                Instant::now(),
            );
            let _ = bot
                .send_message(chat_id, "请输入新的服务器地址（IP 或域名）：")
                .await;
        }
        CallbackAction::ServerEditToken(server_id) => {
            shared.server_edits.lock().await.set(
                chat_id,
                ServerEditMode::Token {
                    server_id: server_id.to_string(),
                },
                Instant::now(),
            );
            let _ = bot.send_message(chat_id, "请输入新的服务器 Token：").await;
        }
        CallbackAction::ServerRevalidate(server_id) => {
            revalidate_server(&bot, chat_id, &shared.config, &shared.timer, server_id).await;
        }
        CallbackAction::ServerDelete(server_id) => {
            let config_snapshot = shared.config.lock().await.clone();
            show_server_delete_confirmation(&bot, chat_id, &config_snapshot, server_id).await;
        }
        CallbackAction::ServerDeleteConfirm(server_id) => {
            delete_server_from_config(&bot, chat_id, &shared.config, &shared.timer, server_id)
                .await;
        }
        CallbackAction::ServerDeleteCancel => {
            let _ = bot.send_message(chat_id, "已取消删除").await;
        }
        CallbackAction::ServerMoveUp(server_id) => {
            move_server(
                &bot,
                chat_id,
                &shared.config,
                &shared.timer,
                server_id,
                true,
            )
            .await;
        }
        CallbackAction::ServerMoveDown(server_id) => {
            move_server(
                &bot,
                chat_id,
                &shared.config,
                &shared.timer,
                server_id,
                false,
            )
            .await;
        }
        CallbackAction::LegacyChange => {
            let _ = bot
                .send_message(chat_id, "旧版 change callback 已拒绝，请重新发送 /change")
                .await;
        }
        CallbackAction::Unknown => {
            let _ = bot
                .send_message(chat_id, "无法识别的操作，请重新发送命令")
                .await;
        }
    }
    Ok(())
}

pub(super) fn parse_callback(data: &str) -> CallbackAction<'_> {
    if data.starts_with("change:") {
        return CallbackAction::LegacyChange;
    }

    match data {
        "menu:addserver" => return CallbackAction::MenuAddServer,
        "menu:servers" => return CallbackAction::MenuServers,
        "menu:status" => return CallbackAction::MenuStatus,
        "menu:change" => return CallbackAction::MenuChange,
        "menu:timer" => return CallbackAction::MenuTimer,
        "menu:help" => return CallbackAction::MenuHelp,
        _ => {}
    }

    if let Some(nonce) = data.strip_prefix("addserver_confirm:") {
        return CallbackAction::ConfirmAddServer(nonce);
    }
    if let Some(nonce) = data.strip_prefix("addserver_retry:") {
        return CallbackAction::RetryAddServer(nonce);
    }
    if let Some(nonce) = data.strip_prefix("addserver_cancel:") {
        return CallbackAction::CancelAddServer(nonce);
    }

    if let Some(server_id) = data.strip_prefix("select_status:") {
        return CallbackAction::SelectStatus(server_id);
    }
    if let Some(server_id) = data.strip_prefix("select_check:") {
        return CallbackAction::SelectCheck(server_id);
    }
    if let Some(server_id) = data.strip_prefix("select_change:") {
        return CallbackAction::SelectChange(server_id);
    }
    if let Some(rest) = data.strip_prefix("confirm_change:") {
        if let Some((server_id, nonce)) = rest.split_once(':') {
            return CallbackAction::ConfirmChange { server_id, nonce };
        }
    }
    if let Some(rest) = data.strip_prefix("cancel_change:") {
        if let Some((server_id, nonce)) = rest.split_once(':') {
            return CallbackAction::CancelChange { server_id, nonce };
        }
    }
    if data == "timer_new" {
        return CallbackAction::TimerNew;
    }
    if data == "timer_edit" {
        return CallbackAction::TimerEdit;
    }
    if data == "timer_close" {
        return CallbackAction::TimerClose;
    }
    if data == "timer_refresh" {
        return CallbackAction::TimerRefresh;
    }
    if let Some(hhmm) = data.strip_prefix("timer_create:all:") {
        return CallbackAction::TimerCreateAll { hhmm };
    }
    if let Some(rest) = data.strip_prefix("timer_create:server:") {
        if let Some((server_id, hhmm)) = rest.split_once(':') {
            return CallbackAction::TimerCreateServer { server_id, hhmm };
        }
    }
    if data == "timer_edit_target:all" {
        return CallbackAction::TimerEditTargetAll;
    }
    if let Some(server_id) = data.strip_prefix("timer_edit_target:server:") {
        return CallbackAction::TimerEditTargetServer(server_id);
    }
    if data == "timer_close:all" {
        return CallbackAction::TimerCloseAll;
    }
    if let Some(server_id) = data.strip_prefix("timer_close:server:") {
        return CallbackAction::TimerCloseServer(server_id);
    }
    if let Some(server_id) = data.strip_prefix("server_edit:") {
        return CallbackAction::ServerEdit(server_id);
    }
    if let Some(server_id) = data.strip_prefix("server_edit_name:") {
        return CallbackAction::ServerEditName(server_id);
    }
    if let Some(server_id) = data.strip_prefix("server_edit_address:") {
        return CallbackAction::ServerEditAddress(server_id);
    }
    if let Some(server_id) = data.strip_prefix("server_edit_token:") {
        return CallbackAction::ServerEditToken(server_id);
    }
    if let Some(server_id) = data.strip_prefix("server_revalidate:") {
        return CallbackAction::ServerRevalidate(server_id);
    }
    if let Some(server_id) = data.strip_prefix("server_delete:") {
        return CallbackAction::ServerDelete(server_id);
    }
    if let Some(server_id) = data.strip_prefix("server_delete_confirm:") {
        return CallbackAction::ServerDeleteConfirm(server_id);
    }
    if data == "server_delete_cancel" {
        return CallbackAction::ServerDeleteCancel;
    }
    if let Some(server_id) = data.strip_prefix("server_move_up:") {
        return CallbackAction::ServerMoveUp(server_id);
    }
    if let Some(server_id) = data.strip_prefix("server_move_down:") {
        return CallbackAction::ServerMoveDown(server_id);
    }
    CallbackAction::Unknown
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confirm_callback_does_not_contain_token() {
        let callback = format!("confirm_change:{}:{}", "hk-01", "nonce-value");
        assert!(!callback.contains("secret-token"));
        assert_eq!(
            parse_callback(&callback),
            CallbackAction::ConfirmChange {
                server_id: "hk-01",
                nonce: "nonce-value"
            }
        );
    }

    #[test]
    fn old_change_callback_is_rejected() {
        assert_eq!(
            parse_callback("change:router:interface"),
            CallbackAction::LegacyChange
        );
    }

    #[test]
    fn timer_callbacks_are_routable_without_tokens() {
        assert_eq!(parse_callback("timer_new"), CallbackAction::TimerNew);
        assert_eq!(parse_callback("timer_edit"), CallbackAction::TimerEdit);
        assert_eq!(parse_callback("timer_close"), CallbackAction::TimerClose);
        assert_eq!(
            parse_callback("timer_refresh"),
            CallbackAction::TimerRefresh
        );
        assert_eq!(
            parse_callback("timer_create:all:03:30"),
            CallbackAction::TimerCreateAll { hhmm: "03:30" }
        );
        assert_eq!(
            parse_callback("timer_create:server:hk-01:03:30"),
            CallbackAction::TimerCreateServer {
                server_id: "hk-01",
                hhmm: "03:30"
            }
        );
        assert_eq!(
            parse_callback("timer_edit_target:server:hk-01"),
            CallbackAction::TimerEditTargetServer("hk-01")
        );
        assert_eq!(
            parse_callback("timer_close:server:hk-01"),
            CallbackAction::TimerCloseServer("hk-01")
        );
        assert_eq!(
            parse_callback("server_edit:hk-01"),
            CallbackAction::ServerEdit("hk-01")
        );
        assert_eq!(
            parse_callback("server_delete:hk-01"),
            CallbackAction::ServerDelete("hk-01")
        );
        assert_eq!(
            parse_callback("server_edit_name:hk-01"),
            CallbackAction::ServerEditName("hk-01")
        );
        assert_eq!(
            parse_callback("server_edit_address:hk-01"),
            CallbackAction::ServerEditAddress("hk-01")
        );
        assert_eq!(
            parse_callback("server_edit_token:hk-01"),
            CallbackAction::ServerEditToken("hk-01")
        );
        assert_eq!(
            parse_callback("server_revalidate:hk-01"),
            CallbackAction::ServerRevalidate("hk-01")
        );
        assert_eq!(
            parse_callback("server_delete_confirm:hk-01"),
            CallbackAction::ServerDeleteConfirm("hk-01")
        );
        assert_eq!(
            parse_callback("server_move_up:hk-01"),
            CallbackAction::ServerMoveUp("hk-01")
        );
        assert_eq!(
            parse_callback("server_move_down:hk-01"),
            CallbackAction::ServerMoveDown("hk-01")
        );
    }
}
