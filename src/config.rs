use anyhow::Context as _;
use dialoguer::{Confirm, Input};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fmt;
use std::path::PathBuf;

const BOIL_SERVERS_ENV: &str = "BOIL_SERVERS";
const BOIL_GLOBAL_TIMER_ENV: &str = "BOIL_GLOBAL_TIMER";

#[derive(Clone, Deserialize, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct SecretToken(String);

impl SecretToken {
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
    pub timer: Option<ServerTimerConfig>,
}

#[derive(Clone, PartialEq, Eq)]
pub struct AppConfig {
    pub servers: Vec<ServerConfig>,
    pub global_timer: Option<ServerTimerConfig>,
    pub tg_token: Option<String>,
    pub tg_chat_id: Option<String>,
    pub migration_notice: Option<String>,
}

impl fmt::Debug for AppConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AppConfig")
            .field("servers", &self.servers)
            .field("global_timer", &self.global_timer)
            .field("tg_token", &self.tg_token.as_ref().map(|_| "<redacted>"))
            .field("tg_chat_id", &self.tg_chat_id)
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
        self.tg_token.is_some() && self.tg_chat_id.is_some()
    }
}

pub fn load_app_config() -> anyhow::Result<AppConfig> {
    let path = config_path();
    if path.exists() {
        dotenvy::from_path(&path).ok();
    }
    dotenvy::dotenv().ok();

    let owned_vars: Vec<(String, String)> = std::env::vars().collect();
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
    let mut replaced = false;
    let mut global_replaced = false;
    let mut lines = Vec::new();

    for line in existing.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with(&format!("{BOIL_SERVERS_ENV}=")) {
            lines.push(servers_line.clone());
            replaced = true;
        } else if trimmed.starts_with(&format!("{BOIL_GLOBAL_TIMER_ENV}=")) {
            if let Some(line) = &global_timer_line {
                lines.push(line.clone());
            }
            global_replaced = true;
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

    let mut content = lines.join("\n");
    content.push('\n');

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("无法创建配置目录: {}", parent.display()))?;
    }

    let temp_path = path.with_extension("env.tmp");
    std::fs::write(&temp_path, content)
        .with_context(|| format!("无法写入临时配置文件: {}", temp_path.display()))?;
    std::fs::rename(&temp_path, path)
        .with_context(|| format!("无法更新配置文件: {}", path.display()))?;

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

/// 构建新版配置内容：只写 BOIL_SERVERS 和 TG 配置，不写旧账号密码。
fn build_config_content(
    existing: &str,
    server_id: &str,
    server_name: &str,
    token: &str,
    tg: Option<(&str, &str)>,
) -> anyhow::Result<String> {
    validate_server_id(server_id)?;
    anyhow::ensure!(!server_name.trim().is_empty(), "server name 不能为空");
    anyhow::ensure!(!token.trim().is_empty(), "token 不能为空");

    let servers = serde_json::json!([
        {
            "id": server_id,
            "name": server_name,
            "token": token,
            "enabled": true
        }
    ]);
    let servers_json = serde_json::to_string_pretty(&servers)?;

    let mut content = format!("BOIL_SERVERS={}\n", shell_single_quote(&servers_json));

    match tg {
        Some((token, chat_id)) => {
            content.push_str(&format!("TG_TOKEN='{token}'\nTG_CHAT_ID='{chat_id}'\n"));
        }
        None => {
            let tg_lines: String = existing
                .lines()
                .filter(|l| l.starts_with("TG_"))
                .map(|l| format!("{l}\n"))
                .collect();
            content.push_str(&tg_lines);
        }
    }
    Ok(content)
}

pub async fn run_setup_wizard() -> anyhow::Result<()> {
    println!("新版 Boil API 使用 Token 配置，一个 Token 对应一台 VPS。");
    println!("请先从 Boil 面板获取该 VPS 的新版 Token。\n");

    let server_id: String = Input::new()
        .with_prompt("server id（字母/数字/-/_，不含敏感信息）")
        .interact_text()?;

    let server_name: String = Input::new().with_prompt("显示名称").interact_text()?;

    let token: String = Input::new()
        .with_prompt("Boil 新版 Token")
        .interact_text()?;

    validate_server_id(&server_id)?;

    let want_tg = Confirm::new()
        .with_prompt("配置 Telegram Bot（用于远程控制）")
        .default(false)
        .interact()?;

    let tg = if want_tg {
        let tg_token: String = Input::new()
            .with_prompt("Bot Token（从 @BotFather 获取）")
            .interact_text()?;
        let chat_id: String = Input::new().with_prompt("TG_CHAT_ID").interact_text()?;
        Some((tg_token, chat_id))
    } else {
        None
    };

    let save_path = setup_save_path();
    let existing = std::fs::read_to_string(&save_path).unwrap_or_default();
    let tg_refs = tg
        .as_ref()
        .map(|(token, chat_id)| (token.as_str(), chat_id.as_str()));
    let content = build_config_content(&existing, &server_id, &server_name, &token, tg_refs)?;
    std::fs::write(&save_path, content)?;
    println!("✅ 新版配置已保存到 {}\n", save_path.display());

    println!("常用命令:");
    println!("  boil servers list          查看 VPS");
    println!("  boil status --server ID    查看当前 IP");
    println!("  boil check --server ID     检查 IP 质量");
    println!("  boil change --server ID    换 IP");
    println!();
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

    /// 复现并验证修复：重新配置 TG 时不应产生重复的 TG_ 行，且新值生效。
    #[test]
    fn reconfigure_tg_no_duplicate() {
        let existing = "BOIL_SERVERS='[]'\nTG_TOKEN='oldtoken'\nTG_CHAT_ID='111'\n";
        let out = build_config_content(
            existing,
            "primary",
            "Primary VPS",
            "new-server-token",
            Some(("newtoken", "222")),
        )
        .unwrap();

        assert_eq!(out.matches("TG_TOKEN=").count(), 1, "TG_TOKEN 应只出现一次");
        assert_eq!(
            out.matches("TG_CHAT_ID=").count(),
            1,
            "TG_CHAT_ID 应只出现一次"
        );
        assert!(out.contains("TG_TOKEN='newtoken'"));
        assert!(out.contains("TG_CHAT_ID='222'"));
        assert!(!out.contains("oldtoken"), "旧 token 不应残留");
        assert!(out.contains("BOIL_SERVERS="));
    }

    /// 跳过 TG 配置时，应保留已有的 TG 配置。
    #[test]
    fn skip_tg_keeps_existing() {
        let existing = "BOIL_SERVERS='[]'\nTG_TOKEN='keep'\nTG_CHAT_ID='1'\n";
        let out = build_config_content(existing, "primary", "Primary VPS", "token", None).unwrap();
        assert!(out.contains("TG_TOKEN='keep'"));
        assert_eq!(out.matches("TG_TOKEN=").count(), 1);
    }

    /// 新版 timer 已进入 BOIL_SERVERS，全局 CHANGE_CRON 不再写入新配置。
    #[test]
    fn does_not_keep_global_cron_when_configuring_new_servers() {
        let existing = "BOIL_SERVERS='[]'\nCHANGE_CRON='0 */6 * * *'\n";
        let out = build_config_content(
            existing,
            "primary",
            "Primary VPS",
            "token",
            Some(("t", "c")),
        )
        .unwrap();
        assert!(!out.contains("CHANGE_CRON="));
    }

    /// token 含单引号时应被正确转义。
    #[test]
    fn escapes_single_quote_in_token() {
        let out = build_config_content("", "primary", "Primary VPS", "to'ken", None).unwrap();
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
