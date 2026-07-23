use std::time::Duration;

pub struct IpQuality {
    pub country: String,
    pub isp: String,
    pub is_proxy: bool,
    pub is_hosting: bool,
}

impl IpQuality {
    pub fn cf_risk(&self) -> &'static str {
        if self.is_proxy || self.is_hosting {
            "高 ⚠️"
        } else {
            "低 ✅"
        }
    }
    pub fn ip_type(&self) -> &'static str {
        if self.is_proxy {
            "代理 ❌"
        } else if self.is_hosting {
            "机房 ⚠️"
        } else {
            "住宅 ✅"
        }
    }
}

pub async fn check_ip_quality(ip: &str) -> Option<IpQuality> {
    let url = format!("http://ip-api.com/json/{ip}?fields=status,country,isp,proxy,hosting");
    let resp: serde_json::Value = reqwest::Client::new()
        .get(&url)
        .timeout(Duration::from_secs(8))
        .send()
        .await
        .ok()?
        .json()
        .await
        .ok()?;

    if resp["status"].as_str() != Some("success") {
        return None;
    }
    Some(IpQuality {
        country: resp["country"].as_str().unwrap_or("未知").to_string(),
        isp: resp["isp"].as_str().unwrap_or("未知").to_string(),
        is_proxy: resp["proxy"].as_bool().unwrap_or(false),
        is_hosting: resp["hosting"].as_bool().unwrap_or(false),
    })
}

pub async fn check_reachable(ip: &str) -> bool {
    for port in [80u16, 443, 22] {
        if tokio::time::timeout(
            Duration::from_secs(3),
            tokio::net::TcpStream::connect(format!("{ip}:{port}")),
        )
        .await
        .map(|r| r.is_ok())
        .unwrap_or(false)
        {
            return true;
        }
    }
    false
}
