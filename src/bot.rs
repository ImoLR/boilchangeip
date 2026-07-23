use std::{
    collections::HashMap,
    sync::atomic::{AtomicU64, Ordering},
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use teloxide::{
    prelude::*,
    types::{BotCommand, InlineKeyboardButton, InlineKeyboardMarkup, MenuButton, ParseMode},
    utils::command::BotCommands,
};

use tokio::sync::Mutex;

use crate::{
    boil::BoilClient,
    config::{AppConfig, ResolvedSelection, ServerConfig, ServerSelection},
    core::check_ip_quality,
    reconnect::{reconnect_one, ReconnectPolicy, ReconnectResult},
    timer::{parse_hhmm, TimerManager, TimerStatus, TimerTarget, TimerUpdate},
};

const CONFIRM_TTL: Duration = Duration::from_secs(120);
const TIMER_INPUT_TTL: Duration = Duration::from_secs(300);
static NONCE_COUNTER: AtomicU64 = AtomicU64::new(1);

#[derive(BotCommands, Clone)]
#[command(rename_rule = "lowercase", description = "可用命令：")]
enum Command {
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
}

#[derive(Clone)]
struct PendingConfirmation {
    server_id: String,
    expires_at: Instant,
    used: bool,
}

#[derive(Default)]
struct ConfirmationStore {
    pending: HashMap<String, PendingConfirmation>,
}

#[derive(Clone)]
enum TimerInputMode {
    New,
    Edit(TimerTarget),
}

#[derive(Clone)]
struct PendingTimerInput {
    mode: TimerInputMode,
    expires_at: Instant,
}

#[derive(Default)]
struct TimerInputStore {
    pending: HashMap<ChatId, PendingTimerInput>,
}

impl TimerInputStore {
    fn set(&mut self, chat_id: ChatId, mode: TimerInputMode, now: Instant) {
        self.prune(now);
        self.pending.insert(
            chat_id,
            PendingTimerInput {
                mode,
                expires_at: now + TIMER_INPUT_TTL,
            },
        );
    }

    fn take(&mut self, chat_id: ChatId, now: Instant) -> Option<TimerInputMode> {
        self.prune(now);
        let pending = self.pending.remove(&chat_id)?;
        (pending.expires_at > now).then_some(pending.mode)
    }

    fn prune(&mut self, now: Instant) {
        self.pending.retain(|_, pending| pending.expires_at > now);
    }
}

impl ConfirmationStore {
    fn insert(&mut self, server_id: &str, now: Instant) -> String {
        self.prune(now);
        let nonce = next_nonce();
        self.pending.insert(
            nonce.clone(),
            PendingConfirmation {
                server_id: server_id.to_string(),
                expires_at: now + CONFIRM_TTL,
                used: false,
            },
        );
        nonce
    }

    fn consume(&mut self, server_id: &str, nonce: &str, now: Instant) -> ConfirmConsume {
        let Some(pending) = self.pending.get_mut(nonce) else {
            self.prune(now);
            return ConfirmConsume::Missing;
        };
        if pending.server_id != server_id {
            self.prune(now);
            return ConfirmConsume::Mismatch;
        }
        if pending.expires_at <= now {
            self.pending.remove(nonce);
            self.prune(now);
            return ConfirmConsume::Expired;
        }
        if pending.used {
            self.prune(now);
            return ConfirmConsume::AlreadyUsed;
        }
        pending.used = true;
        self.prune(now);
        ConfirmConsume::Accepted
    }

    fn prune(&mut self, now: Instant) {
        self.pending.retain(|_, pending| pending.expires_at > now);
    }
}

#[derive(Debug, PartialEq, Eq)]
enum ConfirmConsume {
    Accepted,
    Missing,
    Mismatch,
    Expired,
    AlreadyUsed,
}

#[derive(Debug, PartialEq, Eq)]
enum CallbackAction<'a> {
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
    LegacyChange,
    Unknown,
}

pub async fn run(config: AppConfig) -> anyhow::Result<()> {
    let token = config
        .tg_token
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("未配置 TG_TOKEN，请在 config.env 中配置"))?;

    let bot = Bot::new(token);
    sync_bot_menu(&bot).await;

    let config = Arc::new(config);
    let timer = Arc::new(Mutex::new(TimerManager::new(config.clone()).await?));
    let confirmations = Arc::new(Mutex::new(ConfirmationStore::default()));
    let timer_inputs = Arc::new(Mutex::new(TimerInputStore::default()));

    let handler = dptree::entry()
        .branch(
            Update::filter_message()
                .filter_command::<Command>()
                .endpoint(handle_command),
        )
        .branch(Update::filter_message().endpoint(handle_message))
        .branch(Update::filter_callback_query().endpoint(handle_callback));

    Dispatcher::builder(bot, handler)
        .dependencies(dptree::deps![config, timer, confirmations, timer_inputs])
        .enable_ctrlc_handler()
        .build()
        .dispatch()
        .await;

    Ok(())
}

async fn handle_command(
    bot: Bot,
    msg: Message,
    cmd: Command,
    config: Arc<AppConfig>,
    timer: Arc<Mutex<TimerManager>>,
    confirmations: Arc<Mutex<ConfirmationStore>>,
) -> ResponseResult<()> {
    let chat_id_str = msg.chat.id.to_string();
    if !is_authorized_tg_id(&config, &chat_id_str) {
        return Ok(());
    }

    match cmd {
        Command::Start => {
            bot.send_message(msg.chat.id, start_text()).await?;
        }
        Command::Help => {
            bot.send_message(msg.chat.id, help_text()).await?;
        }
        Command::Status(arg) => tg_status(&bot, msg.chat.id, &config, arg.trim()).await,
        Command::Check(arg) => tg_check(&bot, msg.chat.id, &config, arg.trim()).await,
        Command::Change(arg) => {
            tg_change(&bot, msg.chat.id, &config, &confirmations, arg.trim()).await
        }
        Command::Timer => show_timer_panel(&bot, msg.chat.id, &timer).await,
    }
    Ok(())
}

async fn handle_message(
    bot: Bot,
    msg: Message,
    config: Arc<AppConfig>,
    timer: Arc<Mutex<TimerManager>>,
    timer_inputs: Arc<Mutex<TimerInputStore>>,
) -> ResponseResult<()> {
    let chat_id_str = msg.chat.id.to_string();
    if !is_authorized_tg_id(&config, &chat_id_str) {
        return Ok(());
    }

    let Some(text) = msg.text() else {
        return Ok(());
    };
    let text = text.trim();
    if text.starts_with('/') {
        return Ok(());
    }

    handle_timer_time_input(&bot, msg.chat.id, &timer, &timer_inputs, text).await;
    Ok(())
}

fn menu_commands() -> Vec<BotCommand> {
    Command::bot_commands()
        .into_iter()
        .map(|command| {
            BotCommand::new(command.command.trim_start_matches('/'), command.description)
        })
        .collect()
}

fn help_text() -> String {
    Command::descriptions().to_string()
}

fn start_text() -> String {
    format!("欢迎使用 boilchangeip Telegram Bot。\n\n{}", help_text())
}

async fn sync_bot_menu(bot: &Bot) {
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

async fn sync_menu_step<F, Fut, E>(label: &str, operation: F)
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

async fn handle_callback(
    bot: Bot,
    q: CallbackQuery,
    config: Arc<AppConfig>,
    timer: Arc<Mutex<TimerManager>>,
    confirmations: Arc<Mutex<ConfirmationStore>>,
    timer_inputs: Arc<Mutex<TimerInputStore>>,
) -> ResponseResult<()> {
    let uid = q.from.id.to_string();
    if !is_authorized_tg_id(&config, &uid) {
        bot.answer_callback_query(&q.id).await?;
        return Ok(());
    }
    bot.answer_callback_query(&q.id).await?;

    let chat_id = match &q.message {
        Some(msg) => msg.chat.id,
        None => return Ok(()),
    };

    let Some(data) = q.data.as_deref() else {
        return Ok(());
    };

    match parse_callback(data) {
        CallbackAction::SelectStatus(server_id) => {
            tg_status(&bot, chat_id, &config, server_id).await;
        }
        CallbackAction::SelectCheck(server_id) => {
            tg_check(&bot, chat_id, &config, server_id).await;
        }
        CallbackAction::SelectChange(server_id) => {
            show_change_confirmation(&bot, chat_id, &config, &confirmations, server_id).await;
        }
        CallbackAction::ConfirmChange { server_id, nonce } => {
            confirm_and_change(&bot, chat_id, &config, &confirmations, server_id, nonce).await;
        }
        CallbackAction::CancelChange { server_id, nonce } => {
            let _ = confirmations
                .lock()
                .await
                .consume(server_id, nonce, Instant::now());
            let _ = bot.send_message(chat_id, "已取消换 IP").await;
        }
        CallbackAction::TimerNew => {
            timer_inputs
                .lock()
                .await
                .set(chat_id, TimerInputMode::New, Instant::now());
            let _ = bot
                .send_message(chat_id, "请输入每天执行时间（HH:MM），例如 03:30")
                .await;
        }
        CallbackAction::TimerEdit => {
            show_timer_edit_targets(&bot, chat_id, &timer).await;
        }
        CallbackAction::TimerClose => {
            show_timer_close_targets(&bot, chat_id, &timer).await;
        }
        CallbackAction::TimerRefresh => {
            show_timer_panel(&bot, chat_id, &timer).await;
        }
        CallbackAction::TimerCreateAll { hhmm } => {
            apply_timer_change(
                &bot,
                chat_id,
                &timer,
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
                &timer,
                TimerUpdate::Enable {
                    target: TimerTarget::Server(server_id.to_string()),
                    hhmm: hhmm.to_string(),
                },
            )
            .await;
        }
        CallbackAction::TimerEditTargetAll => {
            timer_inputs.lock().await.set(
                chat_id,
                TimerInputMode::Edit(TimerTarget::AllEnabled),
                Instant::now(),
            );
            let _ = bot
                .send_message(chat_id, "请输入新的每天执行时间（HH:MM），例如 03:30")
                .await;
        }
        CallbackAction::TimerEditTargetServer(server_id) => {
            timer_inputs.lock().await.set(
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
                &timer,
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
                &timer,
                TimerUpdate::Disable {
                    target: TimerTarget::Server(server_id.to_string()),
                },
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

async fn tg_status(bot: &Bot, chat_id: ChatId, config: &AppConfig, arg: &str) {
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
        let text = match client.get_ip(&server.token).await {
            Ok(response) => format!(
                "📡 <b>{}</b>\nserver: <code>{}</code>\nIP: <code>{}</code>",
                html_escape(&server.name),
                html_escape(&server.id),
                response.ip
            ),
            Err(e) => format!(
                "❌ <b>{}</b>\nserver: <code>{}</code>\n查询失败: {}",
                html_escape(&server.name),
                html_escape(&server.id),
                html_escape(&e.to_string())
            ),
        };
        let _ = bot
            .send_message(chat_id, text)
            .parse_mode(ParseMode::Html)
            .await;
    }
}

async fn tg_check(bot: &Bot, chat_id: ChatId, config: &AppConfig, arg: &str) {
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

    for server in selected_servers(selected) {
        let _ = bot
            .send_message(chat_id, format!("🔍 检测中: {}", html_escape(&server.name)))
            .await;
        let response = match client.get_ip(&server.token).await {
            Ok(response) => response,
            Err(e) => {
                let _ = bot
                    .send_message(
                        chat_id,
                        format!("❌ API 查询失败: {}", html_escape(&e.to_string())),
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
        let _ = bot
            .send_message(chat_id, text)
            .parse_mode(ParseMode::Html)
            .await;
    }
}

async fn tg_change(
    bot: &Bot,
    chat_id: ChatId,
    config: &AppConfig,
    confirmations: &Arc<Mutex<ConfirmationStore>>,
    arg: &str,
) {
    let selection = selection_from_tg_arg(arg);
    let selected = match config.resolve_servers(selection) {
        Ok(ResolvedSelection::One(server)) => server,
        Ok(ResolvedSelection::All(_)) => {
            let _ = bot
                .send_message(chat_id, "Telegram 换 IP 不支持 --all，请选择单台 VPS")
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

    show_change_confirmation(bot, chat_id, config, confirmations, &selected.id).await;
}

async fn show_change_confirmation(
    bot: &Bot,
    chat_id: ChatId,
    config: &AppConfig,
    confirmations: &Arc<Mutex<ConfirmationStore>>,
    server_id: &str,
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
    let _ = bot
        .send_message(
            chat_id,
            format!(
                "确认更换 VPS <b>{}</b> 的 IP？\nserver: <code>{}</code>",
                html_escape(&server.name),
                html_escape(&server.id)
            ),
        )
        .reply_markup(keyboard)
        .parse_mode(ParseMode::Html)
        .await;
}

async fn confirm_and_change(
    bot: &Bot,
    chat_id: ChatId,
    config: &AppConfig,
    confirmations: &Arc<Mutex<ConfirmationStore>>,
    server_id: &str,
    nonce: &str,
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

    let _ = bot.send_message(chat_id, "⏳ 开始换 IP，请稍候...").await;
    let client = match BoilClient::new() {
        Ok(client) => client,
        Err(e) => {
            let _ = bot
                .send_message(chat_id, format!("❌ 初始化失败: {e}"))
                .await;
            return;
        }
    };
    let result = reconnect_one(&client, server, &ReconnectPolicy::default()).await;
    send_reconnect_result(bot, chat_id, &result).await;
}

async fn show_timer_panel(bot: &Bot, chat_id: ChatId, timer: &Arc<Mutex<TimerManager>>) {
    let status = timer.lock().await.status();
    let keyboard = InlineKeyboardMarkup::new(vec![
        vec![
            InlineKeyboardButton::callback("➕ 新建", "timer_new"),
            InlineKeyboardButton::callback("✏️ 编辑", "timer_edit"),
        ],
        vec![
            InlineKeyboardButton::callback("⏸ 关闭", "timer_close"),
            InlineKeyboardButton::callback("🔄 刷新", "timer_refresh"),
        ],
    ]);
    let _ = bot
        .send_message(chat_id, format_timer_panel(&status))
        .reply_markup(keyboard)
        .parse_mode(ParseMode::Html)
        .await;
}

async fn show_timer_edit_targets(bot: &Bot, chat_id: ChatId, timer: &Arc<Mutex<TimerManager>>) {
    let config = timer.lock().await.config().clone();
    let keyboard = timer_target_keyboard(&config, "timer_edit_target");
    let _ = bot
        .send_message(chat_id, "请选择要编辑定时时间的范围：")
        .reply_markup(keyboard)
        .await;
}

async fn show_timer_close_targets(bot: &Bot, chat_id: ChatId, timer: &Arc<Mutex<TimerManager>>) {
    let config = timer.lock().await.config().clone();
    let keyboard = timer_target_keyboard(&config, "timer_close");
    let _ = bot
        .send_message(chat_id, "请选择要关闭定时换 IP 的范围：")
        .reply_markup(keyboard)
        .await;
}

async fn handle_timer_time_input(
    bot: &Bot,
    chat_id: ChatId,
    timer: &Arc<Mutex<TimerManager>>,
    timer_inputs: &Arc<Mutex<TimerInputStore>>,
    text: &str,
) {
    let Some(mode) = timer_inputs.lock().await.take(chat_id, Instant::now()) else {
        return;
    };

    if let Err(error) = parse_hhmm(text) {
        let _ = bot
            .send_message(chat_id, format!("❌ {}", html_escape(&error.to_string())))
            .await;
        return;
    }

    match mode {
        TimerInputMode::New => show_timer_create_targets(bot, chat_id, timer, text).await,
        TimerInputMode::Edit(target) => {
            apply_timer_change(
                bot,
                chat_id,
                timer,
                TimerUpdate::Enable {
                    target,
                    hhmm: text.to_string(),
                },
            )
            .await;
        }
    }
}

async fn show_timer_create_targets(
    bot: &Bot,
    chat_id: ChatId,
    timer: &Arc<Mutex<TimerManager>>,
    hhmm: &str,
) {
    let config = timer.lock().await.config().clone();
    let keyboard = timer_create_keyboard(&config, hhmm);
    let _ = bot
        .send_message(chat_id, "请选择定时换 IP 目标：")
        .reply_markup(keyboard)
        .await;
}

async fn apply_timer_change(
    bot: &Bot,
    chat_id: ChatId,
    timer: &Arc<Mutex<TimerManager>>,
    update: TimerUpdate,
) {
    let result = timer.lock().await.apply_update(update).await;
    match result {
        Ok(()) => {
            let _ = bot
                .send_message(chat_id, "✅ 定时配置已保存并重新调度")
                .await;
            show_timer_panel(bot, chat_id, timer).await;
        }
        Err(error) => {
            let _ = bot
                .send_message(
                    chat_id,
                    format!("❌ 保存失败: {}", html_escape(&error.to_string())),
                )
                .await;
        }
    }
}

fn format_timer_panel(status: &TimerStatus) -> String {
    let mut lines = vec![
        "⏰ <b>定时换 IP</b>".to_string(),
        format!("当前时区: <code>{}</code>", html_escape(status.timezone)),
        format!(
            "🌐 全部 Server: {}",
            timer_state_text(status.global_timer_enabled, status.global_time.as_deref())
        ),
        "Server 定时状态:".to_string(),
    ];

    for server in &status.servers {
        let state = if !server.server_enabled {
            "VPS 已禁用".to_string()
        } else if server.timer_enabled {
            timer_state_text(true, server.time.as_deref())
        } else {
            timer_state_text(false, server.time.as_deref())
        };
        lines.push(format!(
            "- 🖥 {} (<code>{}</code>): {}",
            html_escape(&server.server_name),
            html_escape(&server.server_id),
            state
        ));
    }

    lines.join("\n")
}

fn timer_state_text(enabled: bool, time: Option<&str>) -> String {
    if enabled {
        format!(
            "已开启 | 每天 {}",
            html_escape(time.unwrap_or("时间未设置"))
        )
    } else {
        let saved_time = time
            .map(|time| format!(" | 保留时间 {}", html_escape(time)))
            .unwrap_or_default();
        format!("已关闭{saved_time}")
    }
}

fn timer_target_keyboard(config: &AppConfig, prefix: &str) -> InlineKeyboardMarkup {
    let mut rows = vec![vec![InlineKeyboardButton::callback(
        "🌐 全部 Server",
        format!("{prefix}:all"),
    )]];
    rows.extend(enabled_server_buttons(config, prefix));
    InlineKeyboardMarkup::new(rows)
}

fn timer_create_keyboard(config: &AppConfig, hhmm: &str) -> InlineKeyboardMarkup {
    let mut rows = vec![vec![InlineKeyboardButton::callback(
        "🌐 全部 Server",
        format!("timer_create:all:{hhmm}"),
    )]];
    rows.extend(enabled_server_buttons_with_time(
        config,
        "timer_create:server",
        hhmm,
    ));
    InlineKeyboardMarkup::new(rows)
}

fn enabled_server_buttons(config: &AppConfig, prefix: &str) -> Vec<Vec<InlineKeyboardButton>> {
    config
        .servers
        .iter()
        .filter(|server| server.enabled)
        .map(|server| {
            vec![InlineKeyboardButton::callback(
                format!("🖥 {}", server.name),
                format!("{prefix}:server:{}", server.id),
            )]
        })
        .collect()
}

fn enabled_server_buttons_with_time(
    config: &AppConfig,
    prefix: &str,
    hhmm: &str,
) -> Vec<Vec<InlineKeyboardButton>> {
    config
        .servers
        .iter()
        .filter(|server| server.enabled)
        .map(|server| {
            vec![InlineKeyboardButton::callback(
                format!("🖥 {}", server.name),
                format!("{prefix}:{}:{hhmm}", server.id),
            )]
        })
        .collect()
}

async fn resolve_for_tg<'a>(
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

async fn send_reconnect_result(bot: &Bot, chat_id: ChatId, result: &ReconnectResult) {
    let mut lines = vec![
        format!("VPS: <b>{}</b>", html_escape(&result.server_name)),
        format!("server: <code>{}</code>", html_escape(&result.server_id)),
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
    if let Some(next_allowed_at) = result.next_allowed_at {
        lines.push(format!("下次允许时间: {next_allowed_at} (Unix)"));
    }
    if let Some(message) = &result.message {
        lines.push(format!("信息: {}", html_escape(message)));
    }

    let _ = bot
        .send_message(chat_id, lines.join("\n"))
        .parse_mode(ParseMode::Html)
        .await;
}

fn selection_from_tg_arg(arg: &str) -> ServerSelection<'_> {
    let trimmed = arg.trim();
    if trimmed.is_empty() {
        ServerSelection::Unspecified
    } else {
        ServerSelection::Id(trimmed)
    }
}

fn next_nonce() -> String {
    let counter = NONCE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default();
    format!("{millis:x}{counter:x}")
}

fn selected_servers(selected: ResolvedSelection<'_>) -> Vec<&ServerConfig> {
    match selected {
        ResolvedSelection::One(server) => vec![server],
        ResolvedSelection::All(servers) => servers,
    }
}

fn is_authorized_tg_id(config: &AppConfig, id: &str) -> bool {
    config.tg_chat_id.as_deref() == Some(id)
}

fn parse_callback(data: &str) -> CallbackAction<'_> {
    if data.starts_with("change:") {
        return CallbackAction::LegacyChange;
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
    CallbackAction::Unknown
}

fn html_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{SecretToken, ServerTimerConfig};

    fn app_config() -> AppConfig {
        AppConfig {
            servers: vec![
                ServerConfig {
                    id: "hk-01".to_string(),
                    name: "Hong Kong 01".to_string(),
                    token: SecretToken::from_test_value("hidden-token-a"),
                    enabled: true,
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
                    timer: None,
                },
            ],
            global_timer: Some(ServerTimerConfig {
                enabled: true,
                cron: Some("45 4 * * *".to_string()),
            }),
            tg_token: None,
            tg_chat_id: Some("12345".to_string()),
            migration_notice: None,
        }
    }

    #[test]
    fn menu_contains_every_supported_command_with_valid_names() {
        let commands = menu_commands();
        assert_eq!(
            commands
                .iter()
                .map(|command| command.command.as_str())
                .collect::<Vec<_>>(),
            vec!["start", "help", "status", "check", "change", "timer"]
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
    }

    #[test]
    fn timer_panel_shows_timezone_servers_and_actions_without_tokens() {
        let config = app_config();
        let status = crate::timer::timer_status(&config);
        let text = format_timer_panel(&status);

        assert!(text.contains("Asia/Shanghai"));
        assert!(text.contains("🌐 全部 Server"));
        assert!(text.contains("04:45"));
        assert!(text.contains("Hong Kong 01"));
        assert!(text.contains("03:30"));
        assert!(text.contains("Japan 02"));
        assert!(!text.contains("hidden-token"));

        let keyboard = timer_create_keyboard(&config, "03:30");
        let debug = format!("{keyboard:?}");
        assert!(debug.contains("timer_create:all:03:30"));
        assert!(debug.contains("timer_create:server:hk-01:03:30"));
        assert!(!debug.contains("hidden-token"));
    }

    #[test]
    fn timer_input_store_expires_and_consumes_once() {
        let mut store = TimerInputStore::default();
        let chat_id = ChatId(12345);
        let now = Instant::now();

        store.set(chat_id, TimerInputMode::New, now);
        assert!(matches!(
            store.take(chat_id, now + Duration::from_secs(1)),
            Some(TimerInputMode::New)
        ));
        assert!(store.take(chat_id, now + Duration::from_secs(2)).is_none());

        store.set(chat_id, TimerInputMode::New, now);
        assert!(store
            .take(chat_id, now + TIMER_INPUT_TTL + Duration::from_secs(1))
            .is_none());
    }

    #[test]
    fn tg_chat_id_authorization_is_reused_for_timer_inputs_and_callbacks() {
        let config = app_config();
        assert!(is_authorized_tg_id(&config, "12345"));
        assert!(!is_authorized_tg_id(&config, "54321"));
    }

    #[test]
    fn timer_keyboard_has_all_and_single_server_targets() {
        let config = app_config();
        let keyboard = timer_target_keyboard(&config, "timer_close");
        let debug = format!("{keyboard:?}");

        assert!(debug.contains("timer_close:all"));
        assert!(debug.contains("timer_close:server:hk-01"));
        assert!(debug.contains("timer_close:server:jp_02"));
        assert!(!debug.contains("hidden-token"));
    }

    #[test]
    fn nonce_expires_and_cannot_execute() {
        let mut store = ConfirmationStore::default();
        let now = Instant::now();
        let nonce = store.insert("hk-01", now);
        let result = store.consume("hk-01", &nonce, now + CONFIRM_TTL + Duration::from_secs(1));
        assert_eq!(result, ConfirmConsume::Expired);
    }

    #[test]
    fn nonce_is_single_use() {
        let mut store = ConfirmationStore::default();
        let now = Instant::now();
        let nonce = store.insert("hk-01", now);
        assert_eq!(
            store.consume("hk-01", &nonce, now + Duration::from_secs(1)),
            ConfirmConsume::Accepted
        );
        assert_eq!(
            store.consume("hk-01", &nonce, now + Duration::from_secs(2)),
            ConfirmConsume::AlreadyUsed
        );
    }
}
