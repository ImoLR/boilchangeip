use std::net::{IpAddr, ToSocketAddrs};

use crate::{config::ServerConfig, core::check_ip_quality};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct GeoLabel {
    pub(super) flag: String,
    pub(super) country: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct AddressMetadata {
    pub(super) geo: GeoLabel,
    pub(super) resolved_ip: Option<String>,
}

impl GeoLabel {
    pub(super) fn unknown() -> Self {
        Self {
            flag: "🌐".to_string(),
            country: "未知地区".to_string(),
        }
    }

    pub(super) fn display(&self) -> String {
        format!("{} {}", self.flag, self.country)
    }
}

pub(super) fn format_server_card(server: &ServerConfig) -> String {
    let geo = server_geo_label(server);
    let address = server
        .address
        .as_deref()
        .filter(|address| !address.trim().is_empty())
        .unwrap_or("地址未设置");
    format_server_display_parts(&server.name, address, &geo)
}

pub(super) fn format_server_display_parts(name: &str, address: &str, geo: &GeoLabel) -> String {
    format!(
        "📡 <b>{}</b>\n\n{}\n{}",
        html_escape(name),
        html_escape(&geo.display()),
        html_escape(address)
    )
}

pub(super) fn server_geo_label(server: &ServerConfig) -> GeoLabel {
    GeoLabel {
        flag: server.flag.clone().unwrap_or_else(|| "🌐".to_string()),
        country: server
            .country
            .clone()
            .unwrap_or_else(|| "未知地区".to_string()),
    }
}

pub(super) fn normalize_server_address(input: &str) -> Option<String> {
    let mut value = input.trim();
    if value.is_empty() {
        return None;
    }
    value = value
        .strip_prefix("https://")
        .or_else(|| value.strip_prefix("http://"))
        .unwrap_or(value);
    let host = value
        .split('/')
        .next()
        .unwrap_or(value)
        .split(':')
        .next()
        .unwrap_or(value)
        .trim();
    (!host.is_empty()).then(|| host.to_string())
}

pub(super) async fn detect_address_metadata(address: &str) -> AddressMetadata {
    let ip = match resolve_address_ip(address).await {
        Some(ip) => ip,
        None => {
            return AddressMetadata {
                geo: GeoLabel::unknown(),
                resolved_ip: None,
            };
        }
    };

    let ip_string = ip.to_string();
    let geo = check_ip_quality(&ip_string)
        .await
        .map(|quality| geo_from_country(&quality.country))
        .unwrap_or_else(GeoLabel::unknown);

    AddressMetadata {
        geo,
        resolved_ip: Some(ip_string),
    }
}

async fn resolve_address_ip(address: &str) -> Option<IpAddr> {
    if let Ok(ip) = address.parse::<IpAddr>() {
        return Some(ip);
    }

    let lookup = format!("{address}:80");
    tokio::task::spawn_blocking(move || {
        lookup
            .to_socket_addrs()
            .ok()?
            .find(|addr| matches!(addr.ip(), IpAddr::V4(_)))
            .map(|addr| addr.ip())
    })
    .await
    .ok()
    .flatten()
}

fn geo_from_country(country: &str) -> GeoLabel {
    let normalized = country.trim();
    let (flag, country) = match normalized {
        "Japan" | "日本" => ("🇯🇵", "日本"),
        "Singapore" | "新加坡" => ("🇸🇬", "新加坡"),
        "Hong Kong" | "Hong Kong SAR" | "香港" | "中国香港" => ("🇭🇰", "中国香港"),
        "United States" | "美国" => ("🇺🇸", "美国"),
        "Taiwan" | "台湾" | "中国台湾" => ("🇹🇼", "中国台湾"),
        "" => return GeoLabel::unknown(),
        value => ("🌐", value),
    };
    GeoLabel {
        flag: flag.to_string(),
        country: country.to_string(),
    }
}

pub(super) fn short_safe_error(error: &str) -> String {
    let first_line = error.lines().next().unwrap_or("查询失败").trim();
    let sanitized = first_line
        .replace("Authorization", "认证信息")
        .replace("Bearer", "认证信息");
    sanitized.chars().take(80).collect()
}

pub(super) fn html_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bot::test_support::app_config;

    #[test]
    fn server_card_uses_public_display_fields_only() {
        let config = app_config();
        let text = format_server_card(&config.servers[0]);

        assert!(text.contains("📡 <b>Hong Kong 01</b>"));
        assert!(text.contains("🇭🇰 中国香港"));
        assert!(text.contains("203.0.113.10"));
        assert!(!text.contains("hk-01"));
        assert!(!text.contains("hidden-token"));
        assert!(!text.contains("Server:"));
    }
}
