use std::sync::Arc;

use teloxide::{
    prelude::*,
    types::{BotCommand, InlineKeyboardButton, InlineKeyboardMarkup, MenuButton},
    utils::command::BotCommands,
};
use tokio::sync::Mutex;

use crate::config::AppConfig;

use super::{
    change::{tg_change, tg_check},
    pairing::handle_pair_command,
    server_edit::handle_server_edit_input,
    server_list::show_servers,
    server_wizard::{handle_add_server_input, start_add_server_wizard},
    state::BotShared,
    status::tg_status,
    timer_ui::{handle_timer_time_input, show_timer_panel},
};

#[derive(BotCommands, Clone)]
#[command(rename_rule = "lowercase", description = "可用命令：")]
pub(super) enum Command {
    #[command(description = "开始使用 Bot")]
    Start,
    #[command(description = "查看使用帮助")]
    Help,
    #[command(description = "查看 VPS 当前状态")]
    Status(String),
    #[command(description = "检查 VPS 当前 IP 质量")]
    Check(String),
    #[command(description = "更换已启用 VPS 的 IP")]
    Change(String),
    #[command(description = "查看定时换 IP 配置")]
    Timer,
    #[command(description = "查看服务器列表")]
    Servers,
    #[command(description = "添加服务器 Token")]
    Addserver,
    #[command(description = "配对 Telegram Bot")]
    Pair(String),
}

pub(super) async fn handle_command(
    bot: Bot,
    msg: Message,
    cmd: Command,
    shared: BotShared,
) -> ResponseResult<()> {
    if let Command::Pair(code) = cmd {
        handle_pair_command(&bot, msg.chat.id, &shared.config, code.trim()).await;
        return Ok(());
    }

    if !ensure_authorized_message(&msg, &shared.config).await {
        if matches!(cmd, Command::Start) {
            let _ = bot.send_message(msg.chat.id, "请先完成配对").await;
        } else {
            let _ = bot.send_message(msg.chat.id, "拒绝访问").await;
        }
        return Ok(());
    }

    let config_snapshot = shared.config.lock().await.clone();
    match cmd {
        Command::Start => {
            send_start_menu(&bot, msg.chat.id).await?;
        }
        Command::Help => {
            send_help(&bot, msg.chat.id).await?;
        }
        Command::Status(arg) => tg_status(&bot, msg.chat.id, &config_snapshot, arg.trim()).await,
        Command::Check(arg) => tg_check(&bot, msg.chat.id, &config_snapshot, arg.trim()).await,
        Command::Change(arg) => {
            tg_change(
                &bot,
                msg.chat.id,
                &config_snapshot,
                &shared.confirmations,
                arg.trim(),
            )
            .await
        }
        Command::Timer => show_timer_panel(&bot, msg.chat.id, &shared.timer).await,
        Command::Servers => show_servers(&bot, msg.chat.id, &config_snapshot).await,
        Command::Addserver => {
            start_add_server_wizard(&bot, msg.chat.id, &shared.server_wizards).await
        }
        Command::Pair(_) => unreachable!("pair command is handled before authorization"),
    }
    Ok(())
}

pub(super) async fn handle_message(
    bot: Bot,
    msg: Message,
    shared: BotShared,
) -> ResponseResult<()> {
    if !ensure_authorized_message(&msg, &shared.config).await {
        let _ = bot.send_message(msg.chat.id, "拒绝访问").await;
        return Ok(());
    }

    let Some(text) = msg.text() else {
        return Ok(());
    };
    let text = text.trim();
    if text.starts_with('/') {
        return Ok(());
    }

    if handle_server_edit_input(
        &bot,
        msg.chat.id,
        msg.id,
        &shared.config,
        &shared.timer,
        &shared.server_edits,
        text,
    )
    .await
    {
        return Ok(());
    }

    if handle_add_server_input(
        &bot,
        msg.chat.id,
        msg.id,
        &shared.config,
        &shared.timer,
        &shared.server_wizards,
        text,
    )
    .await
    {
        return Ok(());
    }

    handle_timer_time_input(&bot, msg.chat.id, &shared.timer, &shared.timer_inputs, text).await;
    Ok(())
}

pub(super) fn menu_commands() -> Vec<BotCommand> {
    bot_command_specs()
        .iter()
        .map(|(command, description)| BotCommand::new(*command, *description))
        .collect()
}

pub(super) fn help_text() -> String {
    let mut lines = vec!["可用命令：".to_string()];
    lines.extend(
        bot_command_specs()
            .iter()
            .map(|(command, description)| format!("/{command} — {description}")),
    );
    lines.join("\n")
}

fn start_text() -> String {
    "欢迎使用 boilchangeip。\n请选择下面的操作：".to_string()
}

fn bot_command_specs() -> &'static [(&'static str, &'static str)] {
    &[
        ("start", "打开操作菜单"),
        ("status", "查看 VPS 当前状态"),
        ("change", "更换已启用 VPS 的 IP"),
        ("timer", "管理定时换 IP"),
        ("servers", "查看服务器列表"),
        ("addserver", "添加服务器 Token"),
        ("help", "查看使用帮助"),
    ]
}

pub(super) fn start_menu_keyboard() -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![
        vec![
            InlineKeyboardButton::callback("➕ 添加服务器", "menu:addserver"),
            InlineKeyboardButton::callback("🖥 服务器列表", "menu:servers"),
        ],
        vec![
            InlineKeyboardButton::callback("📊 查看状态", "menu:status"),
            InlineKeyboardButton::callback("🔄 更换 IP", "menu:change"),
        ],
        vec![
            InlineKeyboardButton::callback("⏰ 定时任务", "menu:timer"),
            InlineKeyboardButton::callback("❓ 帮助", "menu:help"),
        ],
    ])
}

pub(super) async fn send_start_menu(bot: &Bot, chat_id: ChatId) -> ResponseResult<()> {
    bot.send_message(chat_id, start_text())
        .reply_markup(start_menu_keyboard())
        .await?;
    Ok(())
}

pub(super) async fn send_help(bot: &Bot, chat_id: ChatId) -> ResponseResult<()> {
    bot.send_message(chat_id, help_text()).await?;
    Ok(())
}

pub(super) async fn sync_bot_menu(bot: &Bot) {
    sync_menu_step("Telegram 命令列表", || async {
        bot.set_my_commands(menu_commands()).await.map(|_| ())
    })
    .await;

    sync_menu_step("Telegram 私聊菜单按钮", || async {
        bot.set_chat_menu_button()
            .menu_button(MenuButton::Commands)
            .await
            .map(|_| ())
    })
    .await;
}

pub(super) async fn sync_menu_step<F, Fut, E>(label: &str, operation: F)
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<(), E>>,
    E: std::fmt::Display,
{
    match operation().await {
        Ok(()) => log::info!("{label}同步成功"),
        Err(error) => log::warn!("{label}同步失败，Bot 将继续运行: {error}"),
    }
}

async fn ensure_authorized_message(msg: &Message, config: &Arc<Mutex<AppConfig>>) -> bool {
    let chat_id_str = msg.chat.id.to_string();
    super::pairing::is_authorized_tg_id(&*config.lock().await, &chat_id_str)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{bot::test_support::app_config, config::ServerTimerConfig};

    #[test]
    fn menu_contains_every_supported_command_with_valid_names() {
        let commands = menu_commands();
        assert_eq!(
            commands
                .iter()
                .map(|command| command.command.as_str())
                .collect::<Vec<_>>(),
            vec![
                "start",
                "status",
                "change",
                "timer",
                "servers",
                "addserver",
                "help"
            ]
        );

        for command in commands {
            assert!(!command.command.contains('/'));
            assert!(!command.command.contains(' '));
            assert!(command
                .command
                .chars()
                .all(|character| character.is_ascii_lowercase()
                    || character.is_ascii_digit()
                    || character == '_'));
        }
    }

    #[test]
    fn help_text_uses_the_same_commands_and_descriptions_as_menu() {
        let help = help_text();

        for command in menu_commands() {
            assert!(help.contains(&format!("/{} — {}", command.command, command.description)));
        }
    }

    #[test]
    fn required_commands_are_routable() {
        assert!(matches!(Command::parse("/start", ""), Ok(Command::Start)));
        assert!(matches!(Command::parse("/help", ""), Ok(Command::Help)));
        assert!(matches!(
            Command::parse("/status", ""),
            Ok(Command::Status(argument)) if argument.is_empty()
        ));
        assert!(matches!(
            Command::parse("/change", ""),
            Ok(Command::Change(argument)) if argument.is_empty()
        ));
        assert!(matches!(Command::parse("/timer", ""), Ok(Command::Timer)));
        assert!(matches!(
            Command::parse("/servers", ""),
            Ok(Command::Servers)
        ));
        assert!(matches!(
            Command::parse("/addserver", ""),
            Ok(Command::Addserver)
        ));
        assert!(matches!(
            Command::parse("/pair TEST-CODE", ""),
            Ok(Command::Pair(code)) if code == "TEST-CODE"
        ));
    }

    #[tokio::test]
    async fn failed_menu_sync_does_not_interrupt_startup_flow() {
        let flow_result = async {
            sync_menu_step("测试菜单", || async {
                Err::<(), _>(std::io::Error::other("mock registration failure"))
            })
            .await;
            "dispatcher can continue"
        }
        .await;

        assert_eq!(flow_result, "dispatcher can continue");
    }

    #[test]
    fn start_keyboard_exposes_primary_actions_without_tokens() {
        let debug = format!("{:?}", start_menu_keyboard());

        assert!(debug.contains("➕ 添加服务器"));
        assert!(debug.contains("🖥 服务器列表"));
        assert!(debug.contains("📊 查看状态"));
        assert!(debug.contains("🔄 更换 IP"));
        assert!(debug.contains("⏰ 定时任务"));
        assert!(debug.contains("❓ 帮助"));
        assert!(!debug.contains("hidden-token"));
    }

    #[test]
    fn timer_panel_shows_timezone_servers_and_actions_without_tokens() {
        let config = app_config();
        let status = crate::timer::timer_status(&config);
        let text = super::super::timer_ui::format_timer_panel(&status);

        assert!(text.contains("Asia/Shanghai"));
        assert!(text.contains("🌐 全部 Server"));
        assert!(text.contains("04:45"));
        assert!(text.contains("Hong Kong 01"));
        assert!(text.contains("03:30"));
        assert!(text.contains("Japan 02"));
        assert!(!text.contains("hidden-token"));

        let keyboard = super::super::timer_ui::timer_create_keyboard(&config, "03:30");
        let debug = format!("{keyboard:?}");
        assert!(debug.contains("timer_create:all:03:30"));
        assert!(debug.contains("timer_create:server:hk-01:03:30"));
        assert!(!debug.contains("hidden-token"));
    }

    #[test]
    fn status_countdown_handles_missing_and_paused_timers() {
        let mut config = app_config();
        config.global_timer = None;
        config.servers[0].timer = None;
        assert_eq!(
            super::super::status::status_countdown(&config, &config.servers[0]),
            crate::status_card::CountdownState::NotAvailable
        );

        config.servers[0].timer = Some(ServerTimerConfig {
            enabled: false,
            cron: Some("30 3 * * *".to_string()),
        });
        assert_eq!(
            super::super::status::status_countdown(&config, &config.servers[0]),
            crate::status_card::CountdownState::Paused
        );
    }
}
