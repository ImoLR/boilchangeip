use crate::config::{AppConfig, SecretToken};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerAddressUpdate {
    pub address: String,
    pub country: String,
    pub flag: String,
    pub resolved_ip: Option<String>,
}

pub fn rename_server(config: &mut AppConfig, server_id: &str, name: String) -> anyhow::Result<()> {
    anyhow::ensure!(!name.trim().is_empty(), "服务器名称不能为空");
    let server = server_mut(config, server_id)?;
    server.name = name;
    Ok(())
}

pub fn update_server_address(
    config: &mut AppConfig,
    server_id: &str,
    update: ServerAddressUpdate,
) -> anyhow::Result<()> {
    let server = server_mut(config, server_id)?;
    server.address = Some(update.address);
    server.country = Some(update.country);
    server.flag = Some(update.flag);
    server.resolved_ip = update.resolved_ip;
    Ok(())
}

pub fn update_server_token(
    config: &mut AppConfig,
    server_id: &str,
    token: SecretToken,
) -> anyhow::Result<()> {
    let server = server_mut(config, server_id)?;
    server.token = token;
    Ok(())
}

pub fn delete_server(config: &mut AppConfig, server_id: &str) -> anyhow::Result<()> {
    let index = server_index(config, server_id)?;
    config.servers.remove(index);
    Ok(())
}

pub fn move_server_up(config: &mut AppConfig, server_id: &str) -> anyhow::Result<()> {
    let index = server_index(config, server_id)?;
    if index > 0 {
        config.servers.swap(index - 1, index);
    }
    Ok(())
}

pub fn move_server_down(config: &mut AppConfig, server_id: &str) -> anyhow::Result<()> {
    let index = server_index(config, server_id)?;
    if index + 1 < config.servers.len() {
        config.servers.swap(index, index + 1);
    }
    Ok(())
}

fn server_mut<'a>(
    config: &'a mut AppConfig,
    server_id: &str,
) -> anyhow::Result<&'a mut crate::config::ServerConfig> {
    let index = server_index(config, server_id)?;
    Ok(&mut config.servers[index])
}

fn server_index(config: &AppConfig, server_id: &str) -> anyhow::Result<usize> {
    config
        .servers
        .iter()
        .position(|server| server.id == server_id)
        .ok_or_else(|| anyhow::anyhow!("未找到服务器"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{SecretToken, ServerConfig, ServerTimerConfig};

    fn config() -> AppConfig {
        AppConfig {
            servers: vec![
                ServerConfig {
                    id: "a".to_string(),
                    name: "A".to_string(),
                    token: SecretToken::from_test_value("old-token-a"),
                    enabled: true,
                    address: Some("a.example.com".to_string()),
                    country: Some("中国香港".to_string()),
                    flag: Some("🇭🇰".to_string()),
                    resolved_ip: Some("203.0.113.1".to_string()),
                    timer: Some(ServerTimerConfig {
                        enabled: true,
                        cron: Some("30 3 * * *".to_string()),
                    }),
                },
                ServerConfig {
                    id: "b".to_string(),
                    name: "B".to_string(),
                    token: SecretToken::from_test_value("old-token-b"),
                    enabled: true,
                    address: None,
                    country: None,
                    flag: None,
                    resolved_ip: None,
                    timer: None,
                },
            ],
            global_timer: None,
            tg_token: None,
            tg_chat_id: None,
            tg_pair_code: None,
            tg_pair_expires_at: None,
            migration_notice: None,
        }
    }

    #[test]
    fn renames_server() {
        let mut config = config();
        rename_server(&mut config, "a", "New Name".to_string()).unwrap();
        assert_eq!(config.servers[0].name, "New Name");
    }

    #[test]
    fn updates_address_metadata() {
        let mut config = config();
        update_server_address(
            &mut config,
            "a",
            ServerAddressUpdate {
                address: "hk.example.com".to_string(),
                country: "中国香港".to_string(),
                flag: "🇭🇰".to_string(),
                resolved_ip: Some("203.0.113.10".to_string()),
            },
        )
        .unwrap();
        assert_eq!(config.servers[0].address.as_deref(), Some("hk.example.com"));
        assert_eq!(
            config.servers[0].resolved_ip.as_deref(),
            Some("203.0.113.10")
        );
    }

    #[test]
    fn token_validation_failure_can_keep_old_token() {
        let mut config = config();
        let before = config.servers[0].token.expose_secret().to_string();
        let failed_validation = true;
        if !failed_validation {
            update_server_token(&mut config, "a", SecretToken::from_test_value("new-token"))
                .unwrap();
        }
        assert_eq!(config.servers[0].token.expose_secret(), before);
    }

    #[test]
    fn successful_token_update_replaces_old_token() {
        let mut config = config();
        let before = config.servers[0].token.expose_secret().to_string();

        update_server_token(&mut config, "a", SecretToken::from_test_value("new-token")).unwrap();

        assert_ne!(config.servers[0].token.expose_secret(), before);
        assert_eq!(config.servers[0].token.expose_secret(), "new-token");
    }

    #[test]
    fn deletes_server_and_its_timer() {
        let mut config = config();
        config.global_timer = Some(ServerTimerConfig {
            enabled: true,
            cron: Some("30 3 * * *".to_string()),
        });
        config.servers[1].timer = Some(ServerTimerConfig {
            enabled: true,
            cron: Some("0 8 * * *".to_string()),
        });

        delete_server(&mut config, "a").unwrap();

        assert_eq!(config.servers.len(), 1);
        assert_eq!(config.servers[0].id, "b");
        assert_eq!(
            config
                .global_timer
                .as_ref()
                .and_then(|timer| timer.cron.as_deref()),
            Some("30 3 * * *")
        );
        assert_eq!(
            config.servers[0]
                .timer
                .as_ref()
                .and_then(|timer| timer.cron.as_deref()),
            Some("0 8 * * *")
        );
    }

    #[test]
    fn reorders_servers_in_config_order() {
        let mut config = config();
        move_server_down(&mut config, "a").unwrap();
        assert_eq!(config.servers[0].id, "b");
        move_server_up(&mut config, "a").unwrap();
        assert_eq!(config.servers[0].id, "a");
    }
}
