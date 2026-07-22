mod boil;
mod bot;
mod cli;
mod config;
mod core;
mod reconnect;
mod service;
mod streaming;
mod timer;

use clap::{Parser, Subcommand};
use std::sync::Arc;

#[derive(Parser)]
#[command(name = "boil", about = "Boil.network 换 IP 工具", version)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// 查看配置的 VPS
    Servers {
        #[command(subcommand)]
        action: ServersAction,
    },
    /// 查看当前 IP
    Status(ServerTarget),
    /// 检查当前 IP 质量
    Check(ServerTarget),
    /// 换 IP（重拨）
    Change(ServerTarget),
    /// 后台守护进程：有 TG 则启动机器人，有 cron 则运行定时任务（系统服务使用此命令）
    Daemon,
    /// 启动 Telegram 机器人（需配置 TG）
    Bot,
    /// 重新运行配置向导
    Setup,
    /// 定时换 IP 设置，如: boil timer "0 */6 * * *" 或 boil timer off
    Timer {
        #[command(flatten)]
        target: ServerTarget,
    },
    /// 系统服务管理
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
}

#[derive(Subcommand)]
enum ServersAction {
    /// 列出 server id、名称、启用状态和定时状态
    List,
}

#[derive(clap::Args, Clone, Debug, Default)]
struct ServerTarget {
    /// 指定 server id
    #[arg(long)]
    server: Option<String>,
    /// 顺序操作全部 enabled VPS
    #[arg(long)]
    all: bool,
}

#[derive(Subcommand)]
enum ServiceAction {
    /// 安装并启用 systemd 服务
    Install,
    /// 停止并卸载 systemd 服务
    Uninstall,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();

    let cli = Cli::parse();

    match cli.command {
        None => {
            let config = Arc::new(config::load_app_config()?);
            interactive_menu(config).await?;
        }
        Some(Commands::Servers { action }) => {
            let config = config::load_app_config()?;
            match action {
                ServersAction::List => cli::cmd_servers_list(&config)?,
            }
        }
        Some(Commands::Status(target)) => {
            let config = config::load_app_config()?;
            cli::cmd_status(&config, target.server.as_deref(), target.all).await?;
        }
        Some(Commands::Check(target)) => {
            let config = config::load_app_config()?;
            cli::cmd_check(&config, target.server.as_deref(), target.all).await?;
        }
        Some(Commands::Change(target)) => {
            let config = config::load_app_config()?;
            cli::cmd_change(&config, target.server.as_deref(), target.all).await?;
        }
        Some(Commands::Daemon) => {
            let config = Arc::new(config::load_app_config()?);
            run_daemon(config).await?;
        }
        Some(Commands::Bot) => {
            let config = config::load_app_config()?;
            bot::run(config).await?;
        }
        Some(Commands::Setup) => {
            config::run_setup_wizard().await?;
        }
        Some(Commands::Timer { target }) => {
            let config = config::load_app_config()?;
            let selection = cli::selection_from_args(target.server.as_deref(), target.all)?;
            let _ = config.resolve_servers(selection)?;
            cli::cmd_timer(&config)?;
        }
        Some(Commands::Service { action }) => match action {
            ServiceAction::Install => service::install()?,
            ServiceAction::Uninstall => service::uninstall()?,
        },
    }

    Ok(())
}

/// 系统服务入口：有 TG 跑 bot（含定时器），只有 cron 跑纯定时器，都没有则报错
async fn run_daemon(config: Arc<config::AppConfig>) -> anyhow::Result<()> {
    let has_tg = config.has_tg();
    let has_cron = config.servers.iter().any(|server| {
        server.enabled
            && server
                .timer
                .as_ref()
                .map(|timer| timer.enabled && timer.cron.is_some())
                .unwrap_or(false)
    });

    anyhow::ensure!(
        has_tg || has_cron,
        "守护进程无事可做：请配置 Telegram Bot 或 BOIL_SERVERS 中的 timer"
    );

    if has_tg {
        bot::run((*config).clone()).await?;
    } else {
        println!("定时换 IP 模式启动");
        let _sched = timer::start(config).await?;
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
        }
    }

    Ok(())
}

async fn interactive_menu(config: Arc<config::AppConfig>) -> anyhow::Result<()> {
    use dialoguer::Select;

    let items = vec![
        "📡  status   查看当前 IP",
        "🔍  check    检查 IP 质量和流媒体解锁",
        "🔄  change   换 IP",
        "🖥️   servers  查看 VPS 列表",
        "⏰  timer    查看定时换 IP",
        "⚙️   setup    重新配置",
        "❌  退出",
    ];

    loop {
        let idx = Select::new()
            .with_prompt("Boil — 选择操作")
            .items(&items)
            .default(0)
            .interact()?;

        match idx {
            0 => cli::cmd_status(&config, None, false).await?,
            1 => cli::cmd_check(&config, None, false).await?,
            2 => cli::cmd_change(&config, None, false).await?,
            3 => cli::cmd_servers_list(&config)?,
            4 => cli::cmd_timer(&config)?,
            5 => {
                config::run_setup_wizard().await?;
                break;
            }
            _ => break,
        }
    }

    Ok(())
}
