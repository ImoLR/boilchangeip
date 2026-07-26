use anyhow::Context as _;
use dialoguer::{Confirm, Input};
use rand::{rngs::OsRng, RngCore};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fmt;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};
use teloxide::prelude::*;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

const BOIL_SERVERS_ENV: &str = "BOIL_SERVERS";
const BOIL_GLOBAL_TIMER_ENV: &str = "BOIL_GLOBAL_TIMER";
const TG_PAIR_CODE_ENV: &str = "TG_PAIR_CODE";
const TG_PAIR_EXPIRES_AT_ENV: &str = "TG_PAIR_EXPIRES_AT";
const TG_PAIR_TTL_SECONDS: u64 = 300;

#[derive(Clone, Deserialize, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct SecretToken(String);

impl SecretToken {
    pub fn new(value: String) -> anyhow::Result<Self> {
        anyhow::ensure!(!value.trim().is_empty(), "Token 不能为空");
        Ok(Self(value))
    }

    pub fn expose_secret(&self) -> &str {
        &self.0
    }

    #[cfg(test)]
    pub(crate) fn from_test_value(value: &str) -> Self {
        Self(value.to_string())
    }

    fn is_empty(&self) -> bool {
        self.0.trim().is_empty()
    }
}

impl fmt::Debug for SecretToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

impl fmt::Display for SecretToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

#[derive(Clone, Deserialize, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct PairingCode(String);

impl PairingCode {
    pub fn new(value: String) -> anyhow::Result<Self> {
        let trimmed = value.trim().to_ascii_uppercase();
        anyhow::ensure!(!trimmed.is_empty(), "配对码不能为空");
        Ok(Self(trimmed))
    }

    pub fn expose_secret(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for PairingCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

impl fmt::Display for PairingCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct ServerTimerConfig {
    pub enabled: bool,
    pub cron: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct ServerConfig {
    pub id: String,
    pub name: String,
    pub token: SecretToken,
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub country: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flag: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_ip: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timer: Option<ServerTimerConfig>,
}

#[derive(Clone, PartialEq, Eq)]
pub struct AppConfig {
    pub servers: Vec<ServerConfig>,
    pub global_timer: Option<ServerTimerConfig>,
    pub tg_token: Option<String>,
    pub tg_chat_id: Option<String>,
    pub tg_pair_code: Option<PairingCode>,
    pub tg_pair_expires_at: Option<u64>,
    pub migration_notice: Option<String>,
}

impl fmt::Debug for AppConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AppConfig")
            .field("servers", &self.servers)
            .field("global_timer", &self.global_timer)
            .field("tg_token", &self.tg_token.as_ref().map(|_| "<redacted>"))
            .field("tg_chat_id", &self.tg_chat_id)
            .field(
                "tg_pair_code",
                &self.tg_pair_code.as_ref().map(|_| "<redacted>"),
            )
            .field("tg_pair_expires_at", &self.tg_pair_expires_at)
            .field("migration_notice", &self.migration_notice)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServerSelection<'a> {
    Unspecified,
    Id(&'a str),
    All,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResolvedSelection<'a> {
    One(&'a ServerConfig),
    All(Vec<&'a ServerConfig>),
}

impl AppConfig {
    pub fn from_env_vars<'a, I>(vars: I) -> anyhow::Result<Self>
    where
        I: IntoIterator<Item = (&'a str, &'a str)>,
    {
        let vars: Vec<(&str, &str)> = vars.into_iter().collect();
        let servers_json = vars
            .iter()
            .find_map(|(key, value)| (*key == BOIL_SERVERS_ENV).then_some(*value));

        let Some(servers_json) = servers_json else {
            if has_legacy_boil_config(&vars) {
                return Ok(Self {
                    servers: Vec::new(),
                    global_timer: parse_global_timer(&vars)?,
                    tg_token: find_var(&vars, "TG_TOKEN").map(str::to_string),
                    tg_chat_id: find_var(&vars, "TG_CHAT_ID").map(str::to_string),
                    tg_pair_code: parse_pairing_code(&vars)?,
                    tg_pair_expires_at: parse_pair_expires_at(&vars)?,
                    migration_notice: Some(legacy_config_migration_notice().to_string()),
                });
            }
            anyhow::bail!("缺少 BOIL_SERVERS 配置");
        };

        let servers: Vec<ServerConfig> = serde_json::from_str(servers_json)
            .context("BOIL_SERVERS JSON 解析失败，请检查多 VPS 配置格式")?;
        validate_servers(&servers)?;

        Ok(Self {
            servers,
            global_timer: parse_global_timer(&vars)?,
            tg_token: find_var(&vars, "TG_TOKEN").map(str::to_string),
            tg_chat_id: find_var(&vars, "TG_CHAT_ID").map(str::to_string),
            tg_pair_code: parse_pairing_code(&vars)?,
            tg_pair_expires_at: parse_pair_expires_at(&vars)?,
            migration_notice: None,
        })
    }

    pub fn resolve_servers<'a>(
        &'a self,
        selection: ServerSelection<'_>,
    ) -> anyhow::Result<ResolvedSelection<'a>> {
        resolve_servers(&self.servers, selection)
    }

    pub fn has_tg(&self) -> bool {
        self.tg_token.is_some()
    }
}

pub fn load_app_config() -> anyhow::Result<AppConfig> {
    let path = config_path();
    let mut owned_vars: Vec<(String, String)> = Vec::new();
    if path.exists() {
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("无法读取配置文件: {}", path.display()))?;
        owned_vars.extend(parse_config_env_content(&content)?);
    } else {
        dotenvy::dotenv().ok();
    }
    owned_vars.extend(std::env::vars());
    let borrowed_vars: Vec<(&str, &str)> = owned_vars
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect();
    AppConfig::from_env_vars(borrowed_vars)
}

pub fn save_app_config(config: &AppConfig) -> anyhow::Result<()> {
    save_app_config_to_path(config, &config_path())
}

pub(crate) fn save_app_config_to_path(
    config: &AppConfig,
    path: &std::path::Path,
) -> anyhow::Result<()> {
    validate_servers(&config.servers)?;
    if let Some(timer) = &config.global_timer {
        validate_timer_config(timer).context("BOIL_GLOBAL_TIMER 配置无效")?;
    }

    let existing = std::fs::read_to_string(path).unwrap_or_default();
    let servers_json = serde_json::to_string_pretty(&config.servers)?;
    let servers_line = format!("{BOIL_SERVERS_ENV}={}", shell_single_quote(&servers_json));
    let global_timer_line = match &config.global_timer {
        Some(timer) => {
            let timer_json = serde_json::to_string_pretty(timer)?;
            Some(format!(
                "{BOIL_GLOBAL_TIMER_ENV}={}",
                shell_single_quote(&timer_json)
            ))
        }
        None => None,
    };
    let tg_chat_id_line = config
        .tg_chat_id
        .as_ref()
        .map(|chat_id| format!("TG_CHAT_ID={}", shell_single_quote(chat_id)));
    let tg_pair_code_line = config.tg_pair_code.as_ref().map(|code| {
        format!(
            "{TG_PAIR_CODE_ENV}={}",
            shell_single_quote(code.expose_secret())
        )
    });
    let tg_pair_expires_at_line = config
        .tg_pair_expires_at
        .map(|expires_at| format!("{TG_PAIR_EXPIRES_AT_ENV}={expires_at}"));
    let mut replaced = false;
    let mut global_replaced = false;
    let mut tg_chat_id_replaced = false;
    let mut tg_pair_code_replaced = false;
    let mut tg_pair_expires_at_replaced = false;
    let mut lines = Vec::new();

    let mut existing_lines = existing.lines().peekable();
    while let Some(line) = existing_lines.next() {
        let trimmed = line.trim_start();
        if trimmed.starts_with(&format!("{BOIL_SERVERS_ENV}=")) {
            lines.push(servers_line.clone());
            skip_multiline_single_quoted_value(trimmed, &mut existing_lines);
            replaced = true;
        } else if trimmed.starts_with(&format!("{BOIL_GLOBAL_TIMER_ENV}=")) {
            if let Some(line) = &global_timer_line {
                lines.push(line.clone());
            }
            skip_multiline_single_quoted_value(trimmed, &mut existing_lines);
            global_replaced = true;
        } else if trimmed.starts_with("TG_CHAT_ID=") {
            if let Some(line) = &tg_chat_id_line {
                lines.push(line.clone());
            } else {
                lines.push(line.to_string());
            }
            tg_chat_id_replaced = true;
        } else if trimmed.starts_with(&format!("{TG_PAIR_CODE_ENV}=")) {
            if let Some(line) = &tg_pair_code_line {
                lines.push(line.clone());
            }
            tg_pair_code_replaced = true;
        } else if trimmed.starts_with(&format!("{TG_PAIR_EXPIRES_AT_ENV}=")) {
            if let Some(line) = &tg_pair_expires_at_line {
                lines.push(line.clone());
            }
            tg_pair_expires_at_replaced = true;
        } else {
            lines.push(line.to_string());
        }
    }

    if !replaced {
        lines.insert(0, servers_line);
    }
    if !global_replaced {
        if let Some(line) = global_timer_line {
            lines.insert(1.min(lines.len()), line);
        }
    }
    if !tg_chat_id_replaced {
        if let Some(line) = tg_chat_id_line {
            lines.push(line);
        }
    }
    if !tg_pair_code_replaced {
        if let Some(line) = tg_pair_code_line {
            lines.push(line);
        }
    }
    if !tg_pair_expires_at_replaced {
        if let Some(line) = tg_pair_expires_at_line {
            lines.push(line);
        }
    }

    let mut content = lines.join("\n");
    content.push('\n');

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("无法创建配置目录: {}", parent.display()))?;
    }

    let temp_path = path.with_extension("env.tmp");
    std::fs::write(&temp_path, content)
        .with_context(|| format!("无法写入临时配置文件: {}", temp_path.display()))?;
    set_private_file_permissions(&temp_path)?;
    std::fs::rename(&temp_path, path)
        .with_context(|| format!("无法更新配置文件: {}", path.display()))?;
    set_private_file_permissions(path)?;

    Ok(())
}

#[cfg(unix)]
fn set_private_file_permissions(path: &std::path::Path) -> anyhow::Result<()> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("无法设置配置文件权限: {}", path.display()))
}

#[cfg(not(unix))]
fn set_private_file_permissions(_path: &std::path::Path) -> anyhow::Result<()> {
    Ok(())
}

pub fn resolve_servers<'a>(
    servers: &'a [ServerConfig],
    selection: ServerSelection<'_>,
) -> anyhow::Result<ResolvedSelection<'a>> {
    match selection {
        ServerSelection::Id(id) => {
            let server = servers
                .iter()
                .find(|server| server.id == id)
                .with_context(|| format!("未找到 server id: {id}"))?;
            anyhow::ensure!(server.enabled, "server id '{id}' 已禁用");
            Ok(ResolvedSelection::One(server))
        }
        ServerSelection::All => {
            let enabled = enabled_servers(servers);
            anyhow::ensure!(!enabled.is_empty(), "没有已启用的 VPS");
            Ok(ResolvedSelection::All(enabled))
        }
        ServerSelection::Unspecified => {
            let enabled = enabled_servers(servers);
            match enabled.as_slice() {
                [] => anyhow::bail!("没有已启用的 VPS"),
                [server] => Ok(ResolvedSelection::One(server)),
                [_, _, ..] => {
                    anyhow::bail!("检测到多台已启用 VPS，必须明确指定 server id 或使用 --all")
                }
            }
        }
    }
}

fn enabled_servers(servers: &[ServerConfig]) -> Vec<&ServerConfig> {
    servers.iter().filter(|server| server.enabled).collect()
}

fn validate_servers(servers: &[ServerConfig]) -> anyhow::Result<()> {
    let mut ids = HashSet::new();

    for server in servers {
        validate_server_id(&server.id)?;
        anyhow::ensure!(
            !server.name.trim().is_empty(),
            "server id '{}' 的 name 不能为空",
            server.id
        );
        anyhow::ensure!(
            !server.token.is_empty(),
            "server id '{}' 的 token 不能为空",
            server.id
        );
        anyhow::ensure!(
            !server.id.contains(server.token.expose_secret()),
            "server id '{}' 不得包含 token",
            server.id
        );
        if let Some(timer) = &server.timer {
            validate_timer_config(timer)
                .with_context(|| format!("server id '{}' 的 timer 配置无效", server.id))?;
        }
        anyhow::ensure!(ids.insert(&server.id), "server id '{}' 重复", server.id);
    }

    Ok(())
}

fn validate_server_id(id: &str) -> anyhow::Result<()> {
    anyhow::ensure!(!id.is_empty(), "server id 不能为空");
    anyhow::ensure!(
        id.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
        "server id '{id}' 含非法字符，只允许字母、数字、短横线、下划线"
    );
    Ok(())
}

fn has_legacy_boil_config(vars: &[(&str, &str)]) -> bool {
    [
        "BOIL_ACCOUNT",
        "BOIL_PASSWORD",
        "BOIL_ROUTER_ID",
        "BOIL_INTERFACE",
    ]
    .iter()
    .any(|legacy_key| find_var(vars, legacy_key).is_some())
}

fn parse_pairing_code(vars: &[(&str, &str)]) -> anyhow::Result<Option<PairingCode>> {
    find_var(vars, TG_PAIR_CODE_ENV)
        .map(|code| PairingCode::new(code.to_string()))
        .transpose()
}

fn parse_pair_expires_at(vars: &[(&str, &str)]) -> anyhow::Result<Option<u64>> {
    find_var(vars, TG_PAIR_EXPIRES_AT_ENV)
        .map(|value| {
            value
                .trim()
                .parse::<u64>()
                .context("TG_PAIR_EXPIRES_AT 必须是 Unix 时间戳")
        })
        .transpose()
}

fn parse_global_timer(vars: &[(&str, &str)]) -> anyhow::Result<Option<ServerTimerConfig>> {
    let Some(timer_json) = find_var(vars, BOIL_GLOBAL_TIMER_ENV) else {
        return Ok(None);
    };
    anyhow::ensure!(
        !timer_json.trim().is_empty(),
        "BOIL_GLOBAL_TIMER 不能为空；未启用全局定时时请删除该配置项"
    );
    let timer = serde_json::from_str(timer_json)
        .context("BOIL_GLOBAL_TIMER JSON 解析失败，请检查全局定时配置格式")?;
    validate_timer_config(&timer).context("BOIL_GLOBAL_TIMER 配置无效")?;
    Ok(Some(timer))
}

fn validate_timer_config(timer: &ServerTimerConfig) -> anyhow::Result<()> {
    if let Some(cron) = &timer.cron {
        validate_timer_cron(cron)?;
    }
    Ok(())
}

fn validate_timer_cron(cron: &str) -> anyhow::Result<()> {
    let parts = cron.split_whitespace().collect::<Vec<_>>();
    anyhow::ensure!(parts.len() == 5, "cron 必须是 5 字段格式，例如 30 3 * * *");
    anyhow::ensure!(
        parts.iter().all(|part| !part.trim().is_empty()),
        "cron 字段不能为空"
    );
    Ok(())
}

fn legacy_config_migration_notice() -> &'static str {
    "检测到旧版 Boil 配置（BOIL_ACCOUNT/BOIL_PASSWORD/BOIL_ROUTER_ID/BOIL_INTERFACE）。当前版本已迁移到新版 Token API，不再使用旧账号密码、router_id 或 interface 调用旧 API。请从 Boil 面板获取新版 Token，并为每台 VPS 手动配置 BOIL_SERVERS；不会自动使用旧凭据获取 token。"
}

fn find_var<'a>(vars: &'a [(&str, &str)], key: &str) -> Option<&'a str> {
    vars.iter()
        .find_map(|(candidate, value)| (*candidate == key).then_some(*value))
}

fn config_path() -> PathBuf {
    // 优先级：/etc/boil/ > exe 同目录 > 当前目录
    let candidates = [
        PathBuf::from("/etc/boil/config.env"),
        std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.join("config.env")))
            .unwrap_or_else(|| PathBuf::from("config.env")),
        PathBuf::from("config.env"),
    ];
    candidates
        .into_iter()
        .find(|p| p.exists())
        .unwrap_or_else(|| PathBuf::from("/etc/boil/config.env"))
}

fn parse_config_env_content(content: &str) -> anyhow::Result<Vec<(String, String)>> {
    let mut vars = Vec::new();
    let mut lines = content.lines().enumerate();

    while let Some((line_index, line)) = lines.next() {
        let trimmed = line.trim_start();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        let Some(eq_index) = trimmed.find('=') else {
            anyhow::bail!("配置文件第 {} 行格式无效", line_index + 1);
        };
        let key = trimmed[..eq_index].trim();
        anyhow::ensure!(
            !key.is_empty()
                && key
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || ch == '_'),
            "配置文件第 {} 行变量名无效",
            line_index + 1
        );

        let raw_value = trimmed[eq_index + 1..].trim_start();
        let value = if let Some(mut rest) = raw_value.strip_prefix('\'') {
            let mut value = String::new();
            loop {
                if let Some((before, after)) = split_shell_single_quoted(rest) {
                    value.push_str(before);
                    if let Some(next_rest) = after.strip_prefix("\\''") {
                        value.push('\'');
                        rest = next_rest;
                        continue;
                    }
                    break value;
                }

                value.push_str(rest);
                let Some((_, next_line)) = lines.next() else {
                    anyhow::bail!("配置文件第 {} 行单引号未闭合", line_index + 1);
                };
                value.push('\n');
                rest = next_line;
            }
        } else if raw_value.starts_with('"') && raw_value.ends_with('"') && raw_value.len() >= 2 {
            raw_value[1..raw_value.len() - 1].to_string()
        } else {
            raw_value.trim().to_string()
        };

        vars.push((key.to_string(), value));
    }

    Ok(vars)
}

fn split_shell_single_quoted(value: &str) -> Option<(&str, &str)> {
    value
        .find('\'')
        .map(|quote_index| (&value[..quote_index], &value[quote_index + 1..]))
}

fn skip_multiline_single_quoted_value<'a, I>(line: &str, lines: &mut std::iter::Peekable<I>)
where
    I: Iterator<Item = &'a str>,
{
    let Some(raw_value) = line.split_once('=').map(|(_, value)| value.trim_start()) else {
        return;
    };
    let Some(mut rest) = raw_value.strip_prefix('\'') else {
        return;
    };

    loop {
        if let Some((_, after)) = split_shell_single_quoted(rest) {
            if let Some(next_rest) = after.strip_prefix("\\''") {
                rest = next_rest;
                continue;
            }
            return;
        }

        let Some(next_line) = lines.next() else {
            return;
        };
        rest = next_line;
    }
}

fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// setup 向导写入配置的目标路径（优先写到 /etc/boil/，不存在则写当前目录）
fn setup_save_path() -> PathBuf {
    let etc = PathBuf::from("/etc/boil");
    if etc.exists() || std::fs::create_dir_all(&etc).is_ok() {
        etc.join("config.env")
    } else {
        PathBuf::from("config.env")
    }
}

/// 构建 Telegram 初始化配置：保留已有 BOIL_SERVERS，只写 TG 配置，不写旧账号密码。
struct TgSetupConfig<'a> {
    token: &'a str,
    pair_code: &'a PairingCode,
    pair_expires_at: u64,
}

fn build_tg_config_content(existing: &str, tg: TgSetupConfig<'_>) -> anyhow::Result<String> {
    let vars = parse_config_env_content(existing)?;
    let borrowed = vars
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect::<Vec<_>>();
    let servers_json = find_var(&borrowed, BOIL_SERVERS_ENV).unwrap_or("[]");
    let servers: Vec<ServerConfig> = serde_json::from_str(servers_json)
        .context("已有 BOIL_SERVERS JSON 解析失败，请先修复配置文件")?;
    validate_servers(&servers)?;

    let servers_json = serde_json::to_string_pretty(&servers)?;
    let mut content = format!("BOIL_SERVERS={}\n", shell_single_quote(&servers_json));
    content.push_str(&format!(
        "TG_TOKEN={}\n{TG_PAIR_CODE_ENV}={}\n{TG_PAIR_EXPIRES_AT_ENV}={}\n",
        shell_single_quote(tg.token),
        shell_single_quote(tg.pair_code.expose_secret()),
        tg.pair_expires_at
    ));
    Ok(content)
}

fn generate_pairing_code() -> anyhow::Result<PairingCode> {
    const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let mut bytes = [0_u8; 8];
    OsRng.try_fill_bytes(&mut bytes)?;
    let code = bytes
        .iter()
        .enumerate()
        .map(|(index, byte)| {
            let value = ALPHABET[usize::from(*byte) % ALPHABET.len()] as char;
            if index == 4 {
                format!("-{value}")
            } else {
                value.to_string()
            }
        })
        .collect::<String>();
    PairingCode::new(code)
}

fn pairing_expires_at() -> anyhow::Result<u64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() + TG_PAIR_TTL_SECONDS)
}

async fn validate_tg_token(token: &str) -> anyhow::Result<String> {
    let me = Bot::new(token)
        .get_me()
        .await
        .context("Bot Token 验证失败")?;
    Ok(me.user.username.unwrap_or(me.user.first_name))
}

pub async fn run_setup_wizard() -> anyhow::Result<()> {
    let save_path = setup_save_path();
    let existing = std::fs::read_to_string(&save_path).unwrap_or_default();
    if !existing.trim().is_empty() && std::env::var_os("BOIL_SETUP_FORCE").is_none() {
        println!("检测到已有配置: {}", save_path.display());
        let keep = Confirm::new()
            .with_prompt("是否保留并继续使用现有配置")
            .default(true)
            .interact()?;
        if keep {
            println!("已保留现有配置。");
            return Ok(());
        }
    }

    println!("----------------------------------------");
    println!("Telegram Bot 配置");
    println!("----------------------------------------\n");
    println!("请打开 Telegram，找到：\n");
    println!("@BotFather\n");
    println!("创建一个机器人：\n");
    println!("/newbot\n");
    println!("按照 BotFather 提示完成创建后，复制 Bot Token 并粘贴到这里。\n");

    let (tg_token, bot_name) = loop {
        let tg_token: String = Input::new()
            .with_prompt("Telegram Bot Token")
            .interact_text()?;
        match validate_tg_token(&tg_token).await {
            Ok(bot_name) => break (tg_token, bot_name),
            Err(error) => {
                println!("❌ Bot Token 验证失败: {error}");
                println!("请重新输入 Telegram Bot Token。\n");
            }
        }
    };

    let pair_code = generate_pairing_code()?;
    let pair_expires_at = pairing_expires_at()?;
    let content = build_tg_config_content(
        &existing,
        TgSetupConfig {
            token: tg_token.as_str(),
            pair_code: &pair_code,
            pair_expires_at,
        },
    )?;
    std::fs::write(&save_path, content)?;
    set_private_file_permissions(&save_path)?;
    println!("\nTelegram Bot 验证成功\n");
    println!("机器人：");
    println!("@{}\n", bot_name.trim_start_matches('@'));
    println!("✅ 配置已保存到 {}\n", save_path.display());

    if std::env::var_os("BOIL_SETUP_SUPPRESS_PAIR").is_none() {
        println!("请在 Telegram Bot 中发送：");
        println!("/pair {}", pair_code.expose_secret());
        println!("并等待配对结果。配对码 5 分钟内有效且只能使用一次。\n");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const ONE_SERVER: &str = r#"[
        {
            "id": "primary",
            "name": "Primary VPS",
            "token": "secret-token-primary",
            "enabled": true
        }
    ]"#;

    const TWO_ENABLED_SERVERS: &str = r#"[
        {
            "id": "hk-01",
            "name": "Hong Kong 01",
            "token": "secret-token-hk",
            "enabled": true
        },
        {
            "id": "jp_02",
            "name": "Japan 02",
            "token": "secret-token-jp",
            "enabled": true
        }
    ]"#;

    const MIXED_SERVERS: &str = r#"[
        {
            "id": "hk-01",
            "name": "Hong Kong 01",
            "token": "secret-token-hk",
            "enabled": true
        },
        {
            "id": "disabled",
            "name": "Disabled VPS",
            "token": "secret-token-disabled",
            "enabled": false
        },
        {
            "id": "jp_02",
            "name": "Japan 02",
            "token": "secret-token-jp",
            "enabled": true
        }
    ]"#;

    fn app_from_servers_json(servers_json: &str) -> anyhow::Result<AppConfig> {
        AppConfig::from_env_vars([("BOIL_SERVERS", servers_json)])
    }

    fn selected_one_id(selection: ResolvedSelection<'_>) -> String {
        match selection {
            ResolvedSelection::One(server) => server.id.clone(),
            ResolvedSelection::All(_) => panic!("expected one selected server"),
        }
    }

    fn tg_setup(token: &str) -> TgSetupConfig<'_> {
        let pair_code = Box::leak(Box::new(PairingCode::new("TEST-CODE".to_string()).unwrap()));
        TgSetupConfig {
            token,
            pair_code,
            pair_expires_at: 1_782_732_942,
        }
    }

    /// 复现并验证修复：重新配置 TG 时不应产生重复的 TG_ 行，且使用配对码而不是 Chat ID。
    #[test]
    fn reconfigure_tg_no_duplicate() {
        let existing = "BOIL_SERVERS='[]'\nTG_TOKEN='oldtoken'\nTG_CHAT_ID='111'\n";
        let out = build_tg_config_content(existing, tg_setup("newtoken")).unwrap();

        assert_eq!(out.matches("TG_TOKEN=").count(), 1, "TG_TOKEN 应只出现一次");
        assert_eq!(
            out.matches("TG_CHAT_ID=").count(),
            0,
            "setup 不应直接写入 TG_CHAT_ID"
        );
        assert_eq!(out.matches("TG_PAIR_CODE=").count(), 1);
        assert_eq!(out.matches("TG_PAIR_EXPIRES_AT=").count(), 1);
        assert!(out.contains("TG_TOKEN='newtoken'"));
        assert!(out.contains("TG_PAIR_CODE='TEST-CODE'"));
        assert!(out.contains("TG_PAIR_EXPIRES_AT=1782732942"));
        assert!(!out.contains("oldtoken"), "旧 token 不应残留");
        assert!(out.contains("BOIL_SERVERS="));
    }

    #[test]
    fn setup_preserves_existing_servers() {
        let existing = format!("BOIL_SERVERS={}\n", shell_single_quote(ONE_SERVER));
        let out = build_tg_config_content(&existing, tg_setup("newtoken")).unwrap();

        assert!(out.contains("Primary VPS"));
        assert!(out.contains("secret-token-primary"));
        assert!(out.contains("TG_TOKEN='newtoken'"));
        assert_eq!(out.matches("TG_TOKEN=").count(), 1);
    }

    #[test]
    fn setup_without_existing_servers_writes_empty_server_list() {
        let existing = "BOIL_SERVERS='[]'\nCHANGE_CRON='0 */6 * * *'\n";
        let out = build_tg_config_content(existing, tg_setup("t")).unwrap();

        assert!(out.contains("BOIL_SERVERS='[]'"));
        assert!(!out.contains("CHANGE_CRON="));
    }

    #[test]
    fn escapes_single_quote_in_tg_token() {
        let out = build_tg_config_content("", tg_setup("to'ken")).unwrap();
        assert!(out.contains(r"to'\''ken"));
    }

    #[test]
    fn one_enabled_server_can_be_selected_implicitly() {
        let app = app_from_servers_json(ONE_SERVER).unwrap();
        let selected = app.resolve_servers(ServerSelection::Unspecified).unwrap();
        assert_eq!(selected_one_id(selected), "primary");
    }

    #[test]
    fn multiple_enabled_servers_require_explicit_selection() {
        let app = app_from_servers_json(TWO_ENABLED_SERVERS).unwrap();
        let err = app
            .resolve_servers(ServerSelection::Unspecified)
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("多台已启用 VPS"));
        assert!(msg.contains("--all"));
        assert!(!msg.contains("secret-token-hk"));
        assert!(!msg.contains("secret-token-jp"));
    }

    #[test]
    fn explicit_server_id_selects_matching_enabled_server() {
        let app = app_from_servers_json(TWO_ENABLED_SERVERS).unwrap();
        let selected = app.resolve_servers(ServerSelection::Id("jp_02")).unwrap();
        assert_eq!(selected_one_id(selected), "jp_02");
    }

    #[test]
    fn unknown_server_id_is_rejected() {
        let app = app_from_servers_json(TWO_ENABLED_SERVERS).unwrap();
        let err = app
            .resolve_servers(ServerSelection::Id("missing"))
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("未找到 server id: missing"));
        assert!(!msg.contains("secret-token"));
    }

    #[test]
    fn disabled_server_id_is_rejected() {
        let app = app_from_servers_json(MIXED_SERVERS).unwrap();
        let err = app
            .resolve_servers(ServerSelection::Id("disabled"))
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("已禁用"));
        assert!(!msg.contains("secret-token-disabled"));
    }

    #[test]
    fn all_selection_returns_only_enabled_servers() {
        let app = app_from_servers_json(MIXED_SERVERS).unwrap();
        let selected = app.resolve_servers(ServerSelection::All).unwrap();
        let ResolvedSelection::All(servers) = selected else {
            panic!("expected all selected servers");
        };
        let ids: Vec<&str> = servers.iter().map(|server| server.id.as_str()).collect();
        assert_eq!(ids, vec!["hk-01", "jp_02"]);
    }

    #[test]
    fn all_selection_preserves_config_order() {
        let servers = r#"[
            {
                "id": "third",
                "name": "Third",
                "token": "secret-token-third",
                "enabled": true
            },
            {
                "id": "first",
                "name": "First",
                "token": "secret-token-first",
                "enabled": true
            },
            {
                "id": "second",
                "name": "Second",
                "token": "secret-token-second",
                "enabled": true
            }
        ]"#;
        let app = app_from_servers_json(servers).unwrap();
        let selected = app.resolve_servers(ServerSelection::All).unwrap();
        let ResolvedSelection::All(servers) = selected else {
            panic!("expected all selected servers");
        };
        let ids: Vec<&str> = servers.iter().map(|server| server.id.as_str()).collect();
        assert_eq!(ids, vec!["third", "first", "second"]);
    }

    #[test]
    fn duplicate_server_id_fails_validation() {
        let servers = r#"[
            {
                "id": "dup",
                "name": "One",
                "token": "secret-token-one",
                "enabled": true
            },
            {
                "id": "dup",
                "name": "Two",
                "token": "secret-token-two",
                "enabled": true
            }
        ]"#;
        let err = app_from_servers_json(servers).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("server id 'dup' 重复"));
        assert!(!msg.contains("secret-token-one"));
        assert!(!msg.contains("secret-token-two"));
    }

    #[test]
    fn illegal_server_id_fails_validation() {
        let servers = r#"[
            {
                "id": "bad.id",
                "name": "Bad",
                "token": "secret-token-bad",
                "enabled": true
            }
        ]"#;
        let err = app_from_servers_json(servers).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("含非法字符"));
        assert!(!msg.contains("secret-token-bad"));
    }

    #[test]
    fn debug_and_errors_do_not_include_server_tokens() {
        let app = app_from_servers_json(TWO_ENABLED_SERVERS).unwrap();
        let debug = format!("{app:?}");
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("secret-token-hk"));
        assert!(!debug.contains("secret-token-jp"));

        let err = app
            .resolve_servers(ServerSelection::Unspecified)
            .unwrap_err();
        let error = format!("{err:?}");
        assert!(!error.contains("secret-token-hk"));
        assert!(!error.contains("secret-token-jp"));
    }

    #[test]
    fn legacy_config_gets_migration_prompt_without_using_credentials() {
        let app = AppConfig::from_env_vars([
            ("BOIL_ACCOUNT", "legacy@example.com"),
            ("BOIL_PASSWORD", "legacy-password"),
            ("BOIL_ROUTER_ID", "182"),
            ("BOIL_INTERFACE", "adsl3"),
        ])
        .unwrap();
        assert!(app.servers.is_empty());

        let msg = app.migration_notice.unwrap();
        assert!(msg.contains("旧版 Boil 配置"));
        assert!(msg.contains("当前版本已迁移到新版 Token API"));
        assert!(msg.contains("BOIL_SERVERS"));
        assert!(msg.contains("不会自动使用旧凭据获取 token"));
        assert!(!msg.contains("legacy@example.com"));
        assert!(!msg.contains("legacy-password"));
        assert!(!msg.contains("182"));
        assert!(!msg.contains("adsl3"));
    }

    #[test]
    fn old_boil_servers_config_without_global_timer_still_loads() {
        let app = app_from_servers_json(ONE_SERVER).unwrap();
        assert_eq!(app.servers.len(), 1);
        assert!(app.global_timer.is_none());
    }

    #[test]
    fn multiline_config_keeps_tg_fields_after_boil_servers() {
        let content = format!(
            "BOIL_SERVERS={}\nTG_TOKEN='telegram-token'\nTG_CHAT_ID='12345'\n",
            shell_single_quote(ONE_SERVER)
        );
        let vars = parse_config_env_content(&content).unwrap();
        let borrowed = vars
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str()))
            .collect::<Vec<_>>();
        let app = AppConfig::from_env_vars(borrowed).unwrap();

        assert_eq!(app.servers.len(), 1);
        assert_eq!(app.tg_token.as_deref(), Some("telegram-token"));
        assert_eq!(app.tg_chat_id.as_deref(), Some("12345"));
    }

    #[test]
    fn empty_global_timer_is_configuration_error() {
        let error =
            AppConfig::from_env_vars([("BOIL_SERVERS", ONE_SERVER), ("BOIL_GLOBAL_TIMER", "")])
                .unwrap_err();
        assert!(error.to_string().contains("BOIL_GLOBAL_TIMER 不能为空"));
    }

    #[test]
    fn invalid_global_timer_json_is_configuration_error() {
        let error = AppConfig::from_env_vars([
            ("BOIL_SERVERS", ONE_SERVER),
            ("BOIL_GLOBAL_TIMER", "not-json"),
        ])
        .unwrap_err();
        assert!(error
            .to_string()
            .contains("BOIL_GLOBAL_TIMER JSON 解析失败"));
    }

    #[test]
    fn invalid_timer_cron_is_configuration_error() {
        let servers = r#"[
            {
                "id": "primary",
                "name": "Primary VPS",
                "token": "secret-token-primary",
                "enabled": true,
                "timer": {
                    "enabled": true,
                    "cron": "bad"
                }
            }
        ]"#;
        let error = app_from_servers_json(servers).unwrap_err();
        assert!(error.to_string().contains("timer 配置无效"));
        assert!(!error.to_string().contains("secret-token-primary"));
    }

    #[test]
    fn save_app_config_replaces_only_boil_servers_line() {
        let dir =
            std::env::temp_dir().join(format!("boilchangeip-config-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.env");
        std::fs::write(
            &path,
            "TG_TOKEN='keep'\nBOIL_SERVERS='[]'\nTG_CHAT_ID='123'\n",
        )
        .unwrap();

        let mut app = app_from_servers_json(ONE_SERVER).unwrap();
        app.global_timer = Some(ServerTimerConfig {
            enabled: true,
            cron: Some("45 4 * * *".to_string()),
        });
        app.servers[0].timer = Some(ServerTimerConfig {
            enabled: true,
            cron: Some("30 3 * * *".to_string()),
        });

        save_app_config_to_path(&app, &path).unwrap();
        let saved = std::fs::read_to_string(&path).unwrap();

        assert!(saved.contains("BOIL_SERVERS='"));
        assert!(saved.contains("BOIL_GLOBAL_TIMER='"));
        assert!(saved.contains("\"timer\""));
        assert!(saved.contains("30 3 * * *"));
        assert!(saved.contains("45 4 * * *"));
        assert!(saved.contains("TG_TOKEN='keep'"));
        assert!(saved.contains("TG_CHAT_ID='123'"));
        assert_private_permissions(&path);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn save_app_config_removes_old_multiline_boil_servers_block() {
        let dir = std::env::temp_dir().join(format!(
            "boilchangeip-config-multiline-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.env");
        let original = format!(
            "BOIL_SERVERS={}\nTG_TOKEN='keep'\nTG_CHAT_ID='123'\n",
            shell_single_quote(TWO_ENABLED_SERVERS)
        );
        std::fs::write(&path, original).unwrap();

        let app = app_from_servers_json(ONE_SERVER).unwrap();
        save_app_config_to_path(&app, &path).unwrap();
        let saved = std::fs::read_to_string(&path).unwrap();

        assert_eq!(saved.matches("BOIL_SERVERS=").count(), 1);
        assert!(!saved.contains("Japan 02"));
        assert!(saved.contains("TG_TOKEN='keep'"));
        assert!(saved.contains("TG_CHAT_ID='123'"));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    fn assert_private_permissions(path: &std::path::Path) {
        use std::os::unix::fs::PermissionsExt as _;

        let mode = std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[cfg(not(unix))]
    fn assert_private_permissions(_path: &std::path::Path) {}

    #[test]
    fn save_app_config_file_permissions_are_private() {
        let dir = std::env::temp_dir().join(format!(
            "boilchangeip-config-permission-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.env");
        std::fs::write(&path, "TG_TOKEN='keep'\nBOIL_SERVERS='[]'\n").unwrap();

        let app = app_from_servers_json(ONE_SERVER).unwrap();
        save_app_config_to_path(&app, &path).unwrap();

        assert_private_permissions(&path);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn save_app_config_validation_failure_leaves_existing_file_unchanged() {
        let dir = std::env::temp_dir().join(format!(
            "boilchangeip-config-fail-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.env");
        let original = "TG_TOKEN='keep'\nBOIL_SERVERS='[]'\n";
        std::fs::write(&path, original).unwrap();

        let mut app = app_from_servers_json(ONE_SERVER).unwrap();
        app.global_timer = Some(ServerTimerConfig {
            enabled: true,
            cron: Some("bad".to_string()),
        });

        let error = save_app_config_to_path(&app, &path).unwrap_err();
        assert!(error.to_string().contains("BOIL_GLOBAL_TIMER 配置无效"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
