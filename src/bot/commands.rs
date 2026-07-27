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
    state::{BotShared, UiPage, UiSessionStore},
    status::tg_status,
    timer_ui::{
        handle_timer_time_input, record_sent_page_message, show_timer_panel, TimerUiContext,
    },
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
        handle_pair_command(
            &bot,
            msg.chat.id,
            &shared.config,
            &shared.server_wizards,
            &shared.ui_sessions,
            code.trim(),
        )
        .await;
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

    match cmd {
        Command::Start => {
            send_start_menu(&bot, msg.chat.id, &shared.ui_sessions).await?;
        }
        Command::Help => {
            send_help(&bot, msg.chat.id).await?;
        }
        Command::Status(arg) => {
            let arg = arg.trim().to_string();
            let task_shared = shared.clone();
            let task_bot = bot.clone();
            shared.spawn_if_not_busy(msg.chat.id, async move {
                let config_snapshot = task_shared.config.lock().await.clone();
                tg_status(
                    &task_bot,
                    msg.chat.id,
                    &config_snapshot,
                    &arg,
                    &task_shared.ui_sessions,
                )
                .await;
            });
        }
        Command::Check(arg) => {
            let arg = arg.trim().to_string();
            let task_shared = shared.clone();
            let task_bot = bot.clone();
            shared.spawn_if_not_busy(msg.chat.id, async move {
                let config_snapshot = task_shared.config.lock().await.clone();
                tg_check(
                    &task_bot,
                    msg.chat.id,
                    &config_snapshot,
                    &arg,
                    &task_shared.ui_sessions,
                )
                .await;
            });
        }
        Command::Change(arg) => {
            let arg = arg.trim().to_string();
            let task_shared = shared.clone();
            let task_bot = bot.clone();
            shared.spawn_if_not_busy(msg.chat.id, async move {
                let config_snapshot = task_shared.config.lock().await.clone();
                tg_change(
                    &task_bot,
                    msg.chat.id,
                    &config_snapshot,
                    &task_shared.confirmations,
                    &arg,
                    &task_shared.ui_sessions,
                )
                .await;
            });
        }
        Command::Timer => {
            let task_shared = shared.clone();
            let task_bot = bot.clone();
            shared.spawn_if_not_busy(msg.chat.id, async move {
                show_timer_panel(
                    &task_bot,
                    msg.chat.id,
                    &task_shared.timer,
                    &task_shared.ui_sessions,
                )
                .await;
            });
        }
        Command::Servers => {
            let task_shared = shared.clone();
            let task_bot = bot.clone();
            shared.spawn_if_not_busy(msg.chat.id, async move {
                let config_snapshot = task_shared.config.lock().await.clone();
                show_servers(
                    &task_bot,
                    msg.chat.id,
                    &config_snapshot,
                    &task_shared.ui_sessions,
                )
                .await;
            });
        }
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

    if text.starts_with('/') {
        return Ok(());
    }

    handle_timer_time_input(
        TimerUiContext {
            bot: &bot,
            config: &shared.config,
            timer: &shared.timer,
            ui_sessions: &shared.ui_sessions,
        },
        msg.chat.id,
        msg.id,
        &shared.timer_inputs,
        text,
    )
    .await;
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
            InlineKeyboardButton::callback("⏰ 定时换 IP", "menu:timer"),
            InlineKeyboardButton::callback("❓ 帮助", "menu:help"),
        ],
    ])
}

pub(super) async fn send_start_menu(
    bot: &Bot,
    chat_id: ChatId,
    ui_sessions: &Arc<Mutex<UiSessionStore>>,
) -> ResponseResult<()> {
    let sent = bot
        .send_message(chat_id, start_text())
        .reply_markup(start_menu_keyboard())
        .await;
    record_sent_page_message(bot, chat_id, sent, UiPage::Start, ui_sessions).await;
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
    use crate::bot::test_support::app_config;

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
        assert!(debug.contains("⏰ 定时换 IP"));
        assert!(debug.contains("❓ 帮助"));
        assert!(!debug.contains("hidden-token"));
    }

    #[test]
    fn timer_panel_shows_timezone_servers_and_actions_without_tokens() {
        let config = app_config();
        let status = crate::timer::timer_status(&config);
        let current_ips = std::collections::HashMap::from([
            ("hk-01".to_string(), "203.0.113.10".to_string()),
            ("jp_02".to_string(), "203.0.113.20".to_string()),
        ]);
        let text = super::super::timer_ui::format_timer_panel(&status, &current_ips);

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
}
