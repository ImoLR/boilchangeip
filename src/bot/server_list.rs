use std::{future::Future, sync::Arc};

use teloxide::{prelude::*, types::ParseMode};
use tokio::sync::Mutex;

use crate::{
    config::{AppConfig, ServerConfig},
    provider::ProviderClients,
    server_manage::{move_server_down, move_server_up},
    timer::TimerManager,
};

use super::{
    formatting::{
        detect_address_metadata, format_server_card, format_server_display_parts, html_escape,
        server_geo_label, AddressMetadata, GeoLabel,
    },
    server_edit::save_config_and_reload,
    state::{ServerGeoCache, UiPage, UiSessionStore},
    timer_ui::{record_page_message, record_ui_message},
};

pub(super) async fn show_servers(
    bot: &Bot,
    chat_id: ChatId,
    config: &AppConfig,
    ui_sessions: &Arc<Mutex<UiSessionStore>>,
    geo_cache: &ServerGeoCache,
) {
    if config.servers.is_empty() {
        let sent = bot
            .send_message(chat_id, "尚未添加服务器，请点击“添加服务器”。")
            .await;
        if let Ok(message) = sent {
            record_page_message(bot, chat_id, message.id, UiPage::Servers, ui_sessions).await;
        }
        return;
    }

    let client = match ProviderClients::new() {
        Ok(client) => Some(client),
        Err(error) => {
            log::warn!("服务器列表初始化 Provider 客户端失败: {error}");
            None
        }
    };
    for (index, server) in config.servers.iter().enumerate() {
        let sent = bot
            .send_message(chat_id, format_server_card(server, "⚙️ 正在查询 IP..."))
            .parse_mode(ParseMode::Html)
            .await;
        let message_id = match sent {
            Ok(message) => {
                if index == 0 {
                    record_page_message(bot, chat_id, message.id, UiPage::Servers, ui_sessions)
                        .await;
                } else {
                    record_ui_message(chat_id, message.id, ui_sessions).await;
                }
                Some(message.id)
            }
            Err(error) => {
                log::warn!(
                    "服务器列表发送查询卡片失败: server_id={}: {error}",
                    server.id
                );
                None
            }
        };

        let current_ip = match &client {
            Some(client) => match client.get_ip(server).await {
                Ok(ip) => Some(ip.to_string()),
                Err(error) => {
                    log::warn!(
                        "服务器列表查询当前 IP 失败: server_id={}: {error}",
                        server.id
                    );
                    None
                }
            },
            None => None,
        };

        if let Some(message_id) = message_id {
            let text = match current_ip {
                Some(current_ip) => {
                    let geo = resolve_current_geo(server, &current_ip, geo_cache).await;
                    format_server_display_parts(&server.name, &current_ip, &geo)
                }
                None => format_server_card(server, "查询失败"),
            };
            let _ = bot
                .edit_message_text(chat_id, message_id, text)
                .parse_mode(ParseMode::Html)
                .await;
        }
    }
}

async fn resolve_current_geo(
    server: &ServerConfig,
    current_ip: &str,
    geo_cache: &ServerGeoCache,
) -> GeoLabel {
    resolve_current_geo_with(server, current_ip, geo_cache, |ip| async move {
        detect_address_metadata(&ip).await
    })
    .await
}

async fn resolve_current_geo_with<F, Fut>(
    server: &ServerConfig,
    current_ip: &str,
    geo_cache: &ServerGeoCache,
    lookup: F,
) -> GeoLabel
where
    F: FnOnce(String) -> Fut,
    Fut: Future<Output = AddressMetadata>,
{
    if let Some(geo) = configured_geo_for_current_ip(server, current_ip) {
        return geo;
    }

    if let Some(geo) = runtime_geo_for_current_ip(server, current_ip, geo_cache).await {
        return geo;
    }

    let mut detected = lookup(current_ip.to_string()).await;
    if detected.geo.is_unknown() {
        log::warn!(
            "服务器列表 GEO 查询失败，沿用已保存地区: server_id={}, current_ip={current_ip}",
            server.id
        );
        return server_geo_label(server);
    }

    detected.resolved_ip = Some(current_ip.to_string());
    let geo = detected.geo.clone();
    geo_cache.lock().await.insert(server.id.clone(), detected);
    geo
}

fn configured_geo_for_current_ip(server: &ServerConfig, current_ip: &str) -> Option<GeoLabel> {
    if server.resolved_ip.as_deref() != Some(current_ip) {
        return None;
    }
    complete_server_geo(server)
}

async fn runtime_geo_for_current_ip(
    server: &ServerConfig,
    current_ip: &str,
    geo_cache: &ServerGeoCache,
) -> Option<GeoLabel> {
    let cache = geo_cache.lock().await;
    let metadata = cache.get(&server.id)?;
    if metadata.resolved_ip.as_deref() != Some(current_ip) || metadata.geo.is_unknown() {
        return None;
    }
    Some(metadata.geo.clone())
}

fn complete_server_geo(server: &ServerConfig) -> Option<GeoLabel> {
    let flag = server.flag.as_deref()?.trim();
    let country = server.country.as_deref()?.trim();
    if flag.is_empty() || country.is_empty() {
        return None;
    }
    Some(GeoLabel {
        flag: flag.to_string(),
        country: country.to_string(),
    })
}

pub(super) fn find_configured_server<'a>(
    config: &'a AppConfig,
    server_id: &str,
) -> Option<&'a ServerConfig> {
    config.servers.iter().find(|server| server.id == server_id)
}

pub(super) async fn move_server(
    bot: &Bot,
    chat_id: ChatId,
    config: &Arc<Mutex<AppConfig>>,
    timer: &Arc<Mutex<TimerManager>>,
    server_id: &str,
    up: bool,
) {
    let mut next = config.lock().await.clone();
    let result = if up {
        move_server_up(&mut next, server_id)
    } else {
        move_server_down(&mut next, server_id)
    };
    match result {
        Ok(()) => {
            save_config_and_reload(bot, chat_id, config, timer, next, "✅ 服务器顺序已更新").await;
        }
        Err(error) => {
            let _ = bot
                .send_message(chat_id, format!("❌ {}", html_escape(&error.to_string())))
                .await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bot::test_support::app_config;
    use crate::config::{FacHostSeednetConfig, SecretToken, ServerProvider};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn server_card_has_no_action_buttons_or_missing_address_text() {
        let config = app_config();
        let card = format_server_card(&config.servers[0], "203.0.113.10");

        assert!(card.contains("📡"));
        assert!(card.contains("Hong Kong 01"));
        assert!(card.contains("当前 IP：203.0.113.10"));
        assert!(!card.contains("地址未设置"));
        assert!(!card.contains("📊 状态"));
        assert!(!card.contains("🔄 更换 IP"));
        assert!(!card.contains("编辑"));
        assert!(!card.contains("删除"));
        assert!(!card.contains("hidden-token"));
        assert!(!card.contains("hk-01"));
    }

    #[test]
    fn server_card_supports_fachost_without_token_or_internal_id() {
        let mut config = app_config();
        let server = &mut config.servers[0];
        server.id = "seednet-vds-1".to_string();
        server.name = "Seednet-VDS-1 Black Friday 2025".to_string();
        server.provider = ServerProvider::FachostSeednet;
        server.token = SecretToken::default();
        server.fachost_seednet = Some(FacHostSeednetConfig {
            ssh_host: "seednet.example.com".to_string(),
            ssh_port: 22,
            ssh_user: "root".to_string(),
            ssh_key_path: PathBuf::from("/etc/boil/keys/seednet-vds-1"),
        });
        server.country = Some("中国台湾".to_string());
        server.flag = Some("🇹🇼".to_string());

        let card = format_server_card(server, "112.104.136.174");
        assert!(card.contains("Seednet-VDS-1 Black Friday 2025"));
        assert!(card.contains("🇹🇼 中国台湾"));
        assert!(card.contains("当前 IP：112.104.136.174"));
        assert!(!card.contains("seednet-vds-1"));
        assert!(!card.contains("token"));
    }

    #[tokio::test]
    async fn current_ip_uses_matching_configured_geo_without_lookup() {
        let mut config = app_config();
        let server = &mut config.servers[0];
        server.resolved_ip = Some("203.0.113.10".to_string());
        let cache: ServerGeoCache = Default::default();
        let calls = AtomicUsize::new(0);

        let geo = resolve_current_geo_with(server, "203.0.113.10", &cache, |_| async {
            calls.fetch_add(1, Ordering::SeqCst);
            AddressMetadata {
                geo: GeoLabel::unknown(),
                resolved_ip: None,
            }
        })
        .await;

        assert_eq!(geo.country, "中国香港");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn changed_ip_is_looked_up_once_then_uses_runtime_cache() {
        let config = app_config();
        let server = &config.servers[0];
        let cache: ServerGeoCache = Default::default();
        let calls = AtomicUsize::new(0);

        let first = resolve_current_geo_with(server, "198.51.100.20", &cache, |_| async {
            calls.fetch_add(1, Ordering::SeqCst);
            AddressMetadata {
                geo: GeoLabel {
                    flag: "🇹🇼".to_string(),
                    country: "中国台湾".to_string(),
                },
                resolved_ip: Some("198.51.100.20".to_string()),
            }
        })
        .await;
        let second = resolve_current_geo_with(server, "198.51.100.20", &cache, |_| async {
            calls.fetch_add(1, Ordering::SeqCst);
            AddressMetadata {
                geo: GeoLabel::unknown(),
                resolved_ip: None,
            }
        })
        .await;

        assert_eq!(first.country, "中国台湾");
        assert_eq!(second, first);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn failed_geo_lookup_falls_back_without_caching_unknown() {
        let config = app_config();
        let server = &config.servers[0];
        let cache: ServerGeoCache = Default::default();

        let geo = resolve_current_geo_with(server, "198.51.100.20", &cache, |_| async {
            AddressMetadata {
                geo: GeoLabel::unknown(),
                resolved_ip: Some("198.51.100.20".to_string()),
            }
        })
        .await;

        assert_eq!(geo.country, "中国香港");
        assert!(cache.lock().await.is_empty());
    }
}
