use std::net::IpAddr;

use crate::{
    boil::BoilClient,
    config::{AppConfig, ResolvedSelection, ServerConfig, ServerSelection},
    core::{check_ip_quality, check_reachable},
    reconnect::{
        reconnect_selected, BatchReconnectResult, ReconnectPolicy, ReconnectResult, ReconnectStatus,
    },
};

pub fn selection_from_args<'a>(
    server: Option<&'a str>,
    all: bool,
) -> anyhow::Result<ServerSelection<'a>> {
    anyhow::ensure!(!(server.is_some() && all), "--server 和 --all 不能同时使用");

    Ok(match (server, all) {
        (Some(id), false) => ServerSelection::Id(id),
        (None, true) => ServerSelection::All,
        (None, false) => ServerSelection::Unspecified,
        (Some(_), true) => unreachable!(),
    })
}

pub fn cmd_servers_list(config: &AppConfig) -> anyhow::Result<()> {
    ensure_new_config_ready(config)?;
    println!("VPS 列表\n");
    for server in &config.servers {
        let timer = match &server.timer {
            Some(timer) if timer.enabled => timer.cron.as_deref().unwrap_or("未设置 cron"),
            _ => "未启用",
        };
        let state = if server.enabled {
            "enabled"
        } else {
            "disabled"
        };
        println!(
            "  {} | {} | {} | timer: {}",
            server.id, server.name, state, timer
        );
    }
    Ok(())
}

pub async fn cmd_status(config: &AppConfig, server: Option<&str>, all: bool) -> anyhow::Result<()> {
    ensure_new_config_ready(config)?;
    let selection = selection_from_args(server, all)?;
    let selected = config
        .resolve_servers(selection)
        .map_err(with_server_hint(config))?;
    let client = BoilClient::new()?;

    println!("服务器状态\n");
    for server in selected_servers(selected) {
        match client.get_ip(&server.token).await {
            Ok(response) => {
                println!("  {} ({})\n  IP: {}\n", server.name, server.id, response.ip);
            }
            Err(error) => {
                println!("  {} ({})\n  查询失败: {}\n", server.name, server.id, error);
            }
        }
    }
    Ok(())
}

pub async fn cmd_check(config: &AppConfig, server: Option<&str>, all: bool) -> anyhow::Result<()> {
    ensure_new_config_ready(config)?;
    let selection = selection_from_args(server, all)?;
    let selected = config
        .resolve_servers(selection)
        .map_err(with_server_hint(config))?;
    let client = BoilClient::new()?;
    let servers = selected_servers(selected);

    let mut ips = Vec::new();
    for server in &servers {
        match client.get_ip(&server.token).await {
            Ok(response) => {
                ips.push(response.ip);
                print_quality_for_server(server, response.ip).await;
            }
            Err(error) => {
                println!(
                    "📍 {} ({})\n   API 查询失败: {}\n",
                    server.name, server.id, error
                );
            }
        }
    }

    maybe_print_streaming_check(&ips).await;
    Ok(())
}

pub async fn cmd_change(config: &AppConfig, server: Option<&str>, all: bool) -> anyhow::Result<()> {
    ensure_new_config_ready(config)?;
    let selection = selection_from_args(server, all)?;
    let client = BoilClient::new()?;
    let batch = reconnect_selected(&client, config, selection, &ReconnectPolicy::default())
        .await
        .map_err(with_server_hint(config))?;

    print_batch_result(&batch);
    Ok(())
}

pub fn cmd_timer(config: &AppConfig) -> anyhow::Result<()> {
    ensure_new_config_ready(config)?;
    println!("定时换 IP 配置\n");
    for server in &config.servers {
        let summary = match &server.timer {
            Some(timer) if timer.enabled => match &timer.cron {
                Some(cron) => format!("enabled | {cron}"),
                None => "enabled | cron 未设置".to_string(),
            },
            _ => "disabled".to_string(),
        };
        println!("  {} ({}) | {}", server.name, server.id, summary);
    }
    println!("\n请在 BOIL_SERVERS 中为每台 VPS 配置 timer.enabled 和 timer.cron。");
    Ok(())
}

pub fn print_batch_result(batch: &BatchReconnectResult) {
    println!(
        "换 IP 结果: success={} unconfirmed={} failed={}\n",
        batch.success_count(),
        batch.unconfirmed_count(),
        batch.failure_count()
    );
    for result in &batch.results {
        print_reconnect_result(result);
    }
}

pub fn print_reconnect_result(result: &ReconnectResult) {
    println!("{} ({})", result.server_name, result.server_id);
    println!("  状态: {}", status_text(&result.status));
    if let Some(old_ip) = result.old_ip {
        println!("  旧 IP: {old_ip}");
    }
    if let Some(new_ip) = result.new_ip {
        println!("  新 IP: {new_ip}");
    }
    if let Some(uses_left) = result.uses_left {
        println!("  剩余次数: {uses_left}");
    }
    if let Some(next_allowed_at) = result.next_allowed_at {
        println!("  下次允许时间: {next_allowed_at} (Unix)");
    }
    if let Some(message) = &result.message {
        println!("  信息: {message}");
    }
    println!();
}

fn selected_servers(selected: ResolvedSelection<'_>) -> Vec<&ServerConfig> {
    match selected {
        ResolvedSelection::One(server) => vec![server],
        ResolvedSelection::All(servers) => servers,
    }
}

fn ensure_new_config_ready(config: &AppConfig) -> anyhow::Result<()> {
    if !config.servers.is_empty() {
        return Ok(());
    }

    if let Some(notice) = &config.migration_notice {
        anyhow::bail!("{notice}\n请从 Boil 面板获取新版 Token，并配置 BOIL_SERVERS。");
    }

    anyhow::bail!("缺少 BOIL_SERVERS 配置，请从 Boil 面板获取新版 Token 后配置。")
}

fn with_server_hint<'a>(config: &'a AppConfig) -> impl FnOnce(anyhow::Error) -> anyhow::Error + 'a {
    move |error| {
        let enabled = config
            .servers
            .iter()
            .filter(|server| server.enabled)
            .map(|server| format!("{} ({})", server.id, server.name))
            .collect::<Vec<_>>()
            .join(", ");
        if enabled.is_empty() {
            anyhow::anyhow!("{error}")
        } else {
            anyhow::anyhow!("{error}\n可用 server: {enabled}")
        }
    }
}

async fn print_quality_for_server(server: &ServerConfig, ip: IpAddr) {
    let ip_text = ip.to_string();
    let (reachable, quality) = tokio::join!(check_reachable(&ip_text), check_ip_quality(&ip_text));
    let reach = if reachable {
        "TCP 可达 ✅"
    } else {
        "TCP 未通 ⚠️"
    };
    println!(
        "📍 {} ({})\n   IP: {}  {}",
        server.name, server.id, ip, reach
    );
    if let Some(q) = quality {
        println!(
            "   地区: {} | ISP: {}\n   类型: {} | CF 风险: {}",
            q.country,
            q.isp,
            q.ip_type(),
            q.cf_risk()
        );
    } else {
        println!("   IP 质量检测失败");
    }
    println!();
}

async fn maybe_print_streaming_check(boil_ips: &[IpAddr]) {
    let local_ip = get_local_public_ip().await;
    let on_boil_vps = local_ip
        .as_ref()
        .and_then(|ip| ip.parse::<IpAddr>().ok())
        .map(|ip| boil_ips.iter().any(|candidate| candidate == &ip))
        .unwrap_or(false);

    if on_boil_vps {
        println!("📺 流媒体检测中...");
        let results = crate::streaming::check_all().await;
        for r in &results {
            println!("   {:16} {}", r.service, r.status.display());
        }
    } else {
        println!("📺 流媒体检测跳过（当前运行于非 Boil VPS 机器，结果无意义）");
        println!("   如需检测，请在 Boil VPS 上直接运行 boil check");
    }
    println!();
}

async fn get_local_public_ip() -> Option<String> {
    let client = reqwest::Client::new();
    for url in [
        "https://api.ipify.org",
        "https://ifconfig.me/ip",
        "https://icanhazip.com",
    ] {
        if let Ok(resp) = client
            .get(url)
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await
        {
            if let Ok(text) = resp.text().await {
                let ip = text.trim().to_string();
                if !ip.is_empty() {
                    return Some(ip);
                }
            }
        }
    }
    None
}

fn status_text(status: &ReconnectStatus) -> &'static str {
    match status {
        ReconnectStatus::Success => "成功",
        ReconnectStatus::Disabled => "已禁用",
        ReconnectStatus::PreflightFailed => "预检查失败",
        ReconnectStatus::ApiRejected => "API 拒绝",
        ReconnectStatus::ChangeAcceptedButUnconfirmed => "已接受但未确认",
        ReconnectStatus::InvalidResponse => "响应无效",
        ReconnectStatus::NetworkError => "网络错误",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SecretToken;

    fn app_config() -> AppConfig {
        AppConfig {
            servers: vec![ServerConfig {
                id: "hk-01".to_string(),
                name: "Hong Kong 01".to_string(),
                token: SecretToken::from_test_value("hidden-token"),
                enabled: true,
                timer: None,
            }],
            tg_token: None,
            tg_chat_id: None,
            migration_notice: None,
        }
    }

    #[test]
    fn server_and_all_are_conflicting_selection_options() {
        let error = selection_from_args(Some("hk-01"), true).unwrap_err();
        assert!(error.to_string().contains("--server 和 --all"));
    }

    #[test]
    fn servers_list_output_never_uses_token_debug() {
        let config = app_config();
        let debug = format!("{config:?}");
        assert!(!debug.contains("hidden-token"));
    }

    #[test]
    fn legacy_config_returns_migration_error() {
        let config = AppConfig {
            servers: Vec::new(),
            tg_token: None,
            tg_chat_id: None,
            migration_notice: Some("legacy config".to_string()),
        };
        let error = ensure_new_config_ready(&config).unwrap_err();
        assert!(error.to_string().contains("legacy config"));
        assert!(!error.to_string().contains("BOIL_PASSWORD='"));
    }
}
