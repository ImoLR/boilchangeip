use std::sync::Arc;

use tokio_cron_scheduler::{Job, JobScheduler};

use crate::{boil::BoilClient, config::Config, core::do_reconnect};

/// 启动 cron 调度器，自动换 IP 并通过 TG 通知（如已配置）
pub async fn start(config: Arc<Config>) -> anyhow::Result<JobScheduler> {
    let expr = match &config.change_cron {
        Some(e) => e.clone(),
        None => anyhow::bail!("未配置 CHANGE_CRON"),
    };

    let sched = JobScheduler::new().await?;

    // tokio-cron-scheduler 用 6字段（秒 分 时 日 月 周），我们在前面补 "0 "
    let full_expr = format!("0 {}", expr.trim());
    let cfg = config.clone();

    let job = Job::new_async(&full_expr, move |_uuid, _lock| {
        let cfg = cfg.clone();
        Box::pin(async move {
            run_auto_change(&cfg).await;
        })
    })?;

    sched.add(job).await?;
    sched.start().await?;

    log::info!("定时换 IP 已启动，cron: {expr}");
    Ok(sched)
}

async fn run_auto_change(config: &Config) {
    // 找第一台可换 IP 的服务器
    let target = match get_first_changeable(config).await {
        Ok(Some(t)) => t,
        Ok(None) => {
            log::warn!("定时换 IP：没有可换 IP 的服务器");
            return;
        }
        Err(e) => {
            log::error!("定时换 IP 查询失败: {e}");
            return;
        }
    };

    log::info!("定时换 IP 触发: {}/{}", target.0, target.1);

    match do_reconnect(config, &target.0, &target.1).await {
        Ok(res) => {
            let msg = match &res.new_ip {
                Some(new_ip) => {
                    let quality_info = res.quality.as_ref().map(|q| {
                        format!("\n类型: {} | CF 风险: {}", q.ip_type(), q.cf_risk())
                    }).unwrap_or_default();
                    format!(
                        "⏰ 定时换 IP 完成\n旧 IP: {}\n新 IP: {}{}",
                        res.old_ip.as_deref().unwrap_or("未知"),
                        new_ip,
                        quality_info,
                    )
                }
                None => format!(
                    "⚠️ 定时换 IP：重拨触发但 IP 未变化（旧 IP: {}）",
                    res.old_ip.as_deref().unwrap_or("未知")
                ),
            };
            tg_notify(config, &msg).await;
        }
        Err(e) => {
            tg_notify(config, &format!("❌ 定时换 IP 失败: {e}")).await;
        }
    }
}

async fn get_first_changeable(config: &Config) -> anyhow::Result<Option<(String, String)>> {
    let c = BoilClient::new()?;
    c.login(&config.boil_account, &config.boil_password).await?;
    let data = c.query_all().await?;
    Ok(data.changeable().first().map(|r| (r.router_id.clone(), r.interface.clone())))
}

async fn tg_notify(config: &Config, msg: &str) {
    let (token, chat_id) = match (&config.tg_token, &config.tg_chat_id) {
        (Some(t), Some(c)) => (t, c),
        _ => return,
    };
    let url = format!("https://api.telegram.org/bot{token}/sendMessage");
    let _ = reqwest::Client::new()
        .post(&url)
        .json(&serde_json::json!({ "chat_id": chat_id, "text": msg }))
        .send()
        .await;
}
