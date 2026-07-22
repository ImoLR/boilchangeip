use std::time::Duration;

use reqwest::{redirect::Policy, Client};

pub struct StreamingResult {
    pub service: &'static str,
    pub status: StreamingStatus,
}

pub enum StreamingStatus {
    Unlocked(String), // 解锁，附地区代码
    OriginalsOnly,    // 仅原创内容
    Locked,           // 锁区
    Failed,           // 检测失败
}

impl StreamingStatus {
    pub fn display(&self) -> String {
        match self {
            Self::Unlocked(region) => format!("✅ 解锁 ({})", region),
            Self::OriginalsOnly => "⚠️ 仅原创内容".to_string(),
            Self::Locked => "❌ 锁区".to_string(),
            Self::Failed => "❓ 检测失败".to_string(),
        }
    }
}

fn browser_client() -> reqwest::Result<Client> {
    Client::builder()
        .timeout(Duration::from_secs(10))
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36")
        .build()
}

fn no_redirect_client() -> reqwest::Result<Client> {
    Client::builder()
        .timeout(Duration::from_secs(10))
        .redirect(Policy::none())
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36")
        .build()
}

/// 运行所有流媒体检测
pub async fn check_all() -> Vec<StreamingResult> {
    let (netflix, youtube, disney, spotify, openai) = tokio::join!(
        check_netflix(),
        check_youtube(),
        check_disney_plus(),
        check_spotify(),
        check_openai(),
    );
    vec![netflix, youtube, disney, spotify, openai]
}

async fn check_netflix() -> StreamingResult {
    let status = check_netflix_inner()
        .await
        .unwrap_or(StreamingStatus::Failed);
    StreamingResult {
        service: "Netflix",
        status,
    }
}

async fn check_netflix_inner() -> Option<StreamingStatus> {
    let client = browser_client().ok()?;
    let body = client
        .get("https://www.netflix.com/title/81280792")
        .header("accept-language", "en-US,en;q=0.9")
        .send()
        .await
        .ok()?
        .text()
        .await
        .ok()?;

    if body.contains("Oh no") || body.contains("page-404") {
        return Some(StreamingStatus::OriginalsOnly);
    }

    // 从 HTML 中提取地区代码
    if let Some(region) = extract_netflix_region(&body) {
        return Some(StreamingStatus::Unlocked(region));
    }

    // 检查是否被重定向到登录页（说明 IP 可访问但未登录）
    if body.contains("regRequestToken") || body.contains("login") {
        // 能到登录页说明该地区有 Netflix，尝试从 URL/HTML 提取国家
        if let Some(region) = extract_region_from_html(&body) {
            return Some(StreamingStatus::Unlocked(region));
        }
        return Some(StreamingStatus::Unlocked("?".to_string()));
    }

    Some(StreamingStatus::Failed)
}

fn extract_netflix_region(body: &str) -> Option<String> {
    // 从 HTML 中找 "requestCountry":"XX" 或 "country":"XX"
    for pattern in &[
        r#""requestCountry":""#,
        r#""country":""#,
        r#""geolocation":{"country":""#,
    ] {
        if let Some(pos) = body.find(pattern) {
            let start = pos + pattern.len();
            let end = body[start..].find('"')? + start;
            let code = &body[start..end];
            if code.len() == 2 && code.chars().all(|c| c.is_ascii_alphabetic()) {
                return Some(code.to_uppercase());
            }
        }
    }
    None
}

fn extract_region_from_html(body: &str) -> Option<String> {
    // 尝试从 hreflang 或 og:locale 提取
    if let Some(pos) = body.find("hreflang=\"") {
        let start = pos + 10;
        let end = body[start..].find('"')? + start;
        let lang = &body[start..end]; // 如 "en-HK"
        if lang.len() >= 5 {
            return Some(lang[3..5].to_uppercase());
        }
    }
    None
}

async fn check_youtube() -> StreamingResult {
    let status = check_youtube_inner()
        .await
        .unwrap_or(StreamingStatus::Failed);
    StreamingResult {
        service: "YouTube Premium",
        status,
    }
}

async fn check_youtube_inner() -> Option<StreamingStatus> {
    let client = browser_client().ok()?;
    let resp = client
        .get("https://www.youtube.com/premium")
        .header("accept-language", "en-US,en;q=0.9")
        .send()
        .await
        .ok()?;

    let body = resp.text().await.ok()?;

    if body.contains("www.google.cn") || body.contains("google.com/sorry") {
        return Some(StreamingStatus::Locked);
    }

    if body.contains("Premium is not available in your country") {
        return Some(StreamingStatus::Locked);
    }

    // 提取地区
    let region = extract_youtube_region(&body).unwrap_or_else(|| "?".to_string());

    Some(StreamingStatus::Unlocked(region))
}

fn extract_youtube_region(body: &str) -> Option<String> {
    // INNERTUBE_CONTEXT_GL: "HK"
    if let Some(pos) = body.find("\"INNERTUBE_CONTEXT_GL\":\"") {
        let start = pos + 24;
        let end = body[start..].find('"')? + start;
        let code = &body[start..end];
        if code.len() == 2 {
            return Some(code.to_string());
        }
    }
    None
}

async fn check_disney_plus() -> StreamingResult {
    let status = check_disney_inner()
        .await
        .unwrap_or(StreamingStatus::Failed);
    StreamingResult {
        service: "Disney+",
        status,
    }
}

async fn check_disney_inner() -> Option<StreamingStatus> {
    let client = browser_client().ok()?;
    let resp = client
        .get("https://www.disneyplus.com")
        .header("accept-language", "en-US,en;q=0.9")
        .send()
        .await
        .ok()?;

    let url = resp.url().to_string();
    let body = resp.text().await.ok()?;

    if url.contains("unavailable") || body.contains("not available in your region") {
        return Some(StreamingStatus::Locked);
    }

    // 从最终 URL 提取地区
    let region = url
        .split('/')
        .find(|s| s.len() == 2 && s.chars().all(|c| c.is_ascii_alphabetic()))
        .map(|s| s.to_uppercase())
        .unwrap_or_else(|| "?".to_string());

    Some(StreamingStatus::Unlocked(region))
}

async fn check_spotify() -> StreamingResult {
    let status = check_spotify_inner()
        .await
        .unwrap_or(StreamingStatus::Failed);
    StreamingResult {
        service: "Spotify",
        status,
    }
}

async fn check_spotify_inner() -> Option<StreamingStatus> {
    let client = no_redirect_client().ok()?;
    let resp = client
        .get("https://open.spotify.com")
        .header("accept-language", "en-US,en;q=0.9")
        .send()
        .await
        .ok()?;

    if resp.status().is_success() {
        let body = resp.text().await.ok()?;
        let region = extract_spotify_region(&body).unwrap_or_else(|| "?".to_string());
        return Some(StreamingStatus::Unlocked(region));
    }

    Some(StreamingStatus::Locked)
}

fn extract_spotify_region(body: &str) -> Option<String> {
    // "locale":"en-HK" 或 "country":"HK"
    if let Some(pos) = body.find("\"country\":\"") {
        let start = pos + 11;
        let end = body[start..].find('"')? + start;
        let code = &body[start..end];
        if code.len() == 2 {
            return Some(code.to_uppercase());
        }
    }
    None
}

async fn check_openai() -> StreamingResult {
    let status = check_openai_inner()
        .await
        .unwrap_or(StreamingStatus::Failed);
    StreamingResult {
        service: "ChatGPT",
        status,
    }
}

async fn check_openai_inner() -> Option<StreamingStatus> {
    let client = no_redirect_client().ok()?;
    let resp = client
        .get("https://chat.openai.com")
        .header("accept-language", "en-US,en;q=0.9")
        .send()
        .await
        .ok()?;

    let status = resp.status().as_u16();

    // 200 或 3xx 重定向说明可访问；403/blocked 说明被封
    if status == 403 || status == 451 {
        return Some(StreamingStatus::Locked);
    }

    if status == 200 || (300..400).contains(&status) {
        return Some(StreamingStatus::Unlocked("?".to_string()));
    }

    Some(StreamingStatus::Failed)
}
