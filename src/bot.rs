use std::{
    collections::HashMap,
    sync::atomic::{AtomicU64, Ordering},
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use teloxide::{
    prelude::*,
    types::{InlineKeyboardButton, InlineKeyboardMarkup, ParseMode},
    utils::command::BotCommands,
};

use tokio::sync::Mutex;

use crate::{
    boil::BoilClient,
    config::{AppConfig, ResolvedSelection, ServerConfig, ServerSelection},
    core::check_ip_quality,
    reconnect::{reconnect_one, ReconnectPolicy, ReconnectResult, ReconnectStatus},
    timer::TimerManager,
};

const CONFIRM_TTL: Duration = Duration::from_secs(120);
static NONCE_COUNTER: AtomicU64 = AtomicU64::new(1);

#[derive(BotCommands, Clone)]
#[command(rename_rule = "lowercase", description = "命令列表:")]
enum Command {
    #[command(description = "开始使用")]
    Start,
    #[command(description = "查看当前 IP，可用 /status <server_id>")]
    Status(String),
    #[command(description = "检查当前 IP 质量，可用 /check <server_id>")]
    Check(String),
    #[command(description = "换 IP，可用 /change <server_id>")]
    Change(String),
    #[command(description = "查看定时换 IP")]
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
    LegacyChange,
    Unknown,
}

pub async fn run(config: AppConfig) -> anyhow::Result<()> {
    let token = config
        .tg_token
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("未配置 TG_TOKEN，请在 config.env 中配置"))?;

    let bot = Bot::new(token);
    bot.set_my_commands(Command::bot_commands()).await?;

    let config = Arc::new(config);
    let timer = Arc::new(Mutex::new(TimerManager::new(config.clone()).await?));
    let confirmations = Arc::new(Mutex::new(ConfirmationStore::default()));

    let handler = dptree::entry()
        .branch(
            Update::filter_message()
                .filter_command::<Command>()
                .endpoint(handle_command),
        )
        .branch(Update::filter_callback_query().endpoint(handle_callback));

    Dispatcher::builder(bot, handler)
        .dependencies(dptree::deps![config, timer, confirmations])
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
    if config.tg_chat_id.as_deref() != Some(&chat_id_str) {
        return Ok(());
    }

    match cmd {
        Command::Start => {
            bot.send_message(
                msg.chat.id,
                "👋 <b>Redial Bot</b>\n\n/status — 查看当前 IP\n/check — 检查 IP 质量\n/change — 选择并确认换 IP",
            )
            .parse_mode(ParseMode::Html)
            .await?;
        }
        Command::Status(arg) => tg_status(&bot, msg.chat.id, &config, arg.trim()).await,
        Command::Check(arg) => tg_check(&bot, msg.chat.id, &config, arg.trim()).await,
        Command::Change(arg) => {
            tg_change(&bot, msg.chat.id, &config, &confirmations, arg.trim()).await
        }
        Command::Timer => tg_timer(&bot, msg.chat.id, &timer).await,
    }
    Ok(())
}

async fn handle_callback(
    bot: Bot,
    q: CallbackQuery,
    config: Arc<AppConfig>,
    confirmations: Arc<Mutex<ConfirmationStore>>,
) -> ResponseResult<()> {
    let uid = q.from.id.to_string();
    if config.tg_chat_id.as_deref() != Some(&uid) {
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

async fn tg_timer(bot: &Bot, chat_id: ChatId, timer: &Arc<Mutex<TimerManager>>) {
    let timers = timer.lock().await.current();
    if timers.is_empty() {
        let _ = bot.send_message(chat_id, "⏰ 定时换 IP 未启用").await;
        return;
    }
    let lines = timers
        .iter()
        .map(|(id, name, cron)| {
            format!(
                "{} (<code>{}</code>) | {}",
                html_escape(name),
                html_escape(id),
                html_escape(cron.as_deref().unwrap_or("cron 未设置"))
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let _ = bot
        .send_message(chat_id, format!("⏰ <b>定时换 IP</b>\n{lines}"))
        .parse_mode(ParseMode::Html)
        .await;
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
    if matches!(result.status, ReconnectStatus::ChangeAcceptedButUnconfirmed) {
        lines.push(
            "换 IP 请求已被接受，Boil 后端仍在切换，请稍后使用 `boil status` 或 Telegram `/status` 查看。"
                .to_string(),
        );
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
