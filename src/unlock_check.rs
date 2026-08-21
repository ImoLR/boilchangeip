use std::{path::Path, time::Duration};

use serde_json::Value;
use tokio::{process::Command, time::timeout};

use crate::reconnect::ReconnectStatus;

const DEFAULT_IPQUALITY_SCRIPT: &str = "/etc/boil/ipquality/ip.sh";
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnlockCheckResult {
    pub items: Vec<UnlockCheckItem>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnlockCheckItem {
    pub service: UnlockService,
    pub status: UnlockStatus,
    pub region: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnlockService {
    TikTok,
    DisneyPlus,
    Netflix,
    YouTube,
    AmazonPrimeVideo,
    Reddit,
    ChatGpt,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnlockStatus {
    Unlocked,
    Blocked,
    Failed,
}

#[derive(Debug, PartialEq, Eq)]
pub enum UnlockCheckError {
    Timeout,
    ExecutionFailed(String),
    InvalidJson,
}

impl std::fmt::Display for UnlockCheckError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Timeout => write!(f, "IPQuality 检测超时"),
            Self::ExecutionFailed(message) => write!(f, "IPQuality 执行失败: {message}"),
            Self::InvalidJson => write!(f, "IPQuality JSON 解析失败"),
        }
    }
}

impl std::error::Error for UnlockCheckError {}

impl UnlockService {
    fn json_key(self) -> &'static str {
        match self {
            Self::TikTok => "TikTok",
            Self::DisneyPlus => "DisneyPlus",
            Self::Netflix => "Netflix",
            Self::YouTube => "Youtube",
            Self::AmazonPrimeVideo => "AmazonPrimeVideo",
            Self::Reddit => "Reddit",
            Self::ChatGpt => "ChatGPT",
        }
    }

    fn display_name(self) -> &'static str {
        match self {
            Self::TikTok => "TikTok",
            Self::DisneyPlus => "Disney+",
            Self::Netflix => "Netflix",
            Self::YouTube => "YouTube",
            Self::AmazonPrimeVideo => "Amazon Prime Video",
            Self::Reddit => "Reddit",
            Self::ChatGpt => "ChatGPT",
        }
    }
}

const SERVICES: [UnlockService; 7] = [
    UnlockService::TikTok,
    UnlockService::DisneyPlus,
    UnlockService::Netflix,
    UnlockService::YouTube,
    UnlockService::AmazonPrimeVideo,
    UnlockService::Reddit,
    UnlockService::ChatGpt,
];

pub fn should_run_unlock_check(status: &ReconnectStatus) -> bool {
    *status == ReconnectStatus::Success
}

pub async fn run_ipv4_unlock_check() -> Result<UnlockCheckResult, UnlockCheckError> {
    let script =
        std::env::var("BOIL_IPQUALITY_SCRIPT").unwrap_or_else(|_| DEFAULT_IPQUALITY_SCRIPT.into());
    run_ipv4_unlock_check_with_script(script, DEFAULT_TIMEOUT).await
}

pub async fn run_ipv4_unlock_check_message() -> String {
    match run_ipv4_unlock_check().await {
        Ok(result) => format_unlock_check_result(&result),
        Err(error) => {
            log::warn!("IPQuality IPv4 解锁检测失败: {error}");
            format_unlock_check_failure()
        }
    }
}

pub async fn run_ipv4_unlock_check_with_script(
    script: impl AsRef<Path>,
    timeout_duration: Duration,
) -> Result<UnlockCheckResult, UnlockCheckError> {
    let future = Command::new("timeout")
        .arg(format_timeout_arg(timeout_duration))
        .arg("bash")
        .arg(script.as_ref())
        .args(["-4", "-j", "-p", "-n"])
        .output();
    let output = timeout(timeout_duration + Duration::from_secs(5), future)
        .await
        .map_err(|_| UnlockCheckError::Timeout)?
        .map_err(|error| UnlockCheckError::ExecutionFailed(error.to_string()))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    if !output.status.success() {
        if let Ok(result) = parse_ipquality_json(&stdout) {
            return Ok(result);
        }
        if output.status.code() == Some(124) {
            return Err(UnlockCheckError::Timeout);
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        let message = stderr
            .lines()
            .find(|line| !line.trim().is_empty())
            .unwrap_or("exit status indicates failure")
            .chars()
            .take(160)
            .collect();
        return Err(UnlockCheckError::ExecutionFailed(message));
    }

    parse_ipquality_json(&stdout)
}

fn format_timeout_arg(duration: Duration) -> String {
    if duration.subsec_millis() == 0 {
        duration.as_secs().to_string()
    } else {
        format!("{:.3}s", duration.as_secs_f64())
    }
}

pub fn parse_ipquality_json(input: &str) -> Result<UnlockCheckResult, UnlockCheckError> {
    let json = extract_json_object(input).ok_or(UnlockCheckError::InvalidJson)?;
    let root = serde_json::from_str::<Value>(json).map_err(|_| UnlockCheckError::InvalidJson)?;
    let media = root.get("Media").and_then(Value::as_object);
    let items = SERVICES
        .into_iter()
        .map(|service| {
            let item = media.and_then(|media| media.get(service.json_key()));
            UnlockCheckItem {
                service,
                status: item
                    .and_then(|value| value.get("Status"))
                    .and_then(Value::as_str)
                    .map(parse_status)
                    .unwrap_or(UnlockStatus::Failed),
                region: item
                    .and_then(|value| value.get("Region"))
                    .and_then(Value::as_str)
                    .and_then(normalize_region),
            }
        })
        .collect();

    Ok(UnlockCheckResult { items })
}

pub fn format_unlock_check_result(result: &UnlockCheckResult) -> String {
    let mut lines = vec!["🌐 解锁检测".to_string(), String::new()];
    for item in &result.items {
        lines.push(format!(
            "{}：{}",
            item.service.display_name(),
            format_unlock_item(item)
        ));
    }
    lines.join("\n")
}

pub fn format_unlock_check_failure() -> String {
    "🌐 解锁检测\n\n⚠️ 检测失败".to_string()
}

fn format_unlock_item(item: &UnlockCheckItem) -> String {
    match item.status {
        UnlockStatus::Unlocked => match &item.region {
            Some(region) => format!("✅ {region}"),
            None => "✅ 解锁".to_string(),
        },
        UnlockStatus::Blocked => "❌ 未解锁".to_string(),
        UnlockStatus::Failed => "⚠️ 检测失败".to_string(),
    }
}

fn extract_json_object(input: &str) -> Option<&str> {
    let start = input.find('{')?;
    let end = input.rfind('}')?;
    (start <= end).then(|| &input[start..=end])
}

fn parse_status(status: &str) -> UnlockStatus {
    let normalized = status
        .trim()
        .trim_matches('.')
        .to_ascii_lowercase()
        .replace([' ', '-'], "");
    match normalized.as_str() {
        "yes" | "unlocked" | "unlock" | "native" | "解锁" | "原生" => UnlockStatus::Unlocked,
        "block" | "blocked" | "no" | "noprem" | "china" | "未解锁" | "不支持" => {
            UnlockStatus::Blocked
        }
        _ => UnlockStatus::Failed,
    }
}

fn normalize_region(region: &str) -> Option<String> {
    let region = region.trim().trim_matches(['[', ']']);
    if region.is_empty()
        || region.eq_ignore_ascii_case("null")
        || region.eq_ignore_ascii_case("n/a")
        || region.eq_ignore_ascii_case("unknown")
    {
        return None;
    }

    Some(
        match region.to_ascii_uppercase().as_str() {
            "TW" => "台湾",
            "HK" => "香港",
            "JP" => "日本",
            "SG" => "新加坡",
            "US" => "美国",
            "CN" => "中国",
            "KR" => "韩国",
            "GB" | "UK" => "英国",
            "DE" => "德国",
            "FR" => "法国",
            "CA" => "加拿大",
            "AU" => "澳大利亚",
            "NL" => "荷兰",
            "TH" => "泰国",
            "MY" => "马来西亚",
            "ID" => "印度尼西亚",
            "PH" => "菲律宾",
            _ => region,
        }
        .to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    fn sample_json() -> &'static str {
        r#"{
          "Media": {
            "TikTok": {"Status": "Yes", "Region": "TW", "Type": "Native"},
            "DisneyPlus": {"Status": "Yes", "Region": "TW", "Type": "Native"},
            "Netflix": {"Status": "Yes", "Region": "TW", "Type": "Native"},
            "Youtube": {"Status": "Yes", "Region": "TW", "Type": "Native"},
            "AmazonPrimeVideo": {"Status": "Yes", "Region": "TW", "Type": "Native"},
            "Reddit": {"Status": "Yes", "Region": "", "Type": ""},
            "ChatGPT": {"Status": "Yes", "Region": "TW", "Type": "Native"}
          }
        }"#
    }

    #[test]
    fn success_status_is_the_only_unlock_trigger() {
        assert!(should_run_unlock_check(&ReconnectStatus::Success));
        assert!(!should_run_unlock_check(
            &ReconnectStatus::ChangeAcceptedButUnconfirmed
        ));
        assert!(!should_run_unlock_check(&ReconnectStatus::ApiRejected));
    }

    #[test]
    fn parses_media_unlock_regions() {
        let result = parse_ipquality_json(sample_json()).unwrap();
        let text = format_unlock_check_result(&result);

        assert!(text.contains("Netflix：✅ 台湾"));
        assert!(text.contains("Reddit：✅ 解锁"));
        assert!(text.contains("ChatGPT：✅ 台湾"));
    }

    #[test]
    fn blocked_status_is_displayed_as_not_unlocked() {
        let result = parse_ipquality_json(
            r#"{"Media":{"Netflix":{"Status":"Block","Region":"","Type":""}}}"#,
        )
        .unwrap();

        assert!(format_unlock_check_result(&result).contains("Netflix：❌ 未解锁"));
    }

    #[test]
    fn parses_real_ipquality_chinese_unlock_status() {
        let result = parse_ipquality_json(
            r#"{"Media":{"Netflix":{"Status":"解锁","Region":"JP","Type":"原生"}}}"#,
        )
        .unwrap();

        assert!(format_unlock_check_result(&result).contains("Netflix：✅ 日本"));
    }

    #[test]
    fn real_ipquality_failure_status_is_item_failure_not_blocked() {
        let result =
            parse_ipquality_json(r#"{"Media":{"TikTok":{"Status":"失败","Region":"","Type":""}}}"#)
                .unwrap();

        assert!(format_unlock_check_result(&result).contains("TikTok：⚠️ 检测失败"));
    }

    #[test]
    fn missing_service_does_not_fail_whole_parse() {
        let result = parse_ipquality_json(r#"{"Media":{"Netflix":{"Status":"Yes"}}}"#).unwrap();

        assert_eq!(result.items.len(), 7);
        assert_eq!(result.items[0].status, UnlockStatus::Failed);
        assert_eq!(result.items[2].status, UnlockStatus::Unlocked);
    }

    #[tokio::test]
    async fn execution_failure_is_reported_without_panicking() {
        let result =
            run_ipv4_unlock_check_with_script("/path/that/does/not/exist", Duration::from_secs(1))
                .await;

        assert!(matches!(result, Err(UnlockCheckError::ExecutionFailed(_))));
    }

    #[tokio::test]
    async fn execution_timeout_is_reported_without_panicking() {
        let dir = unique_test_dir();
        fs::create_dir_all(&dir).unwrap();
        let script = dir.join("sleep.sh");
        fs::write(&script, "#!/usr/bin/env bash\nsleep 2\n").unwrap();

        let result = run_ipv4_unlock_check_with_script(&script, Duration::from_millis(50)).await;

        assert_eq!(result, Err(UnlockCheckError::Timeout));
        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn json_stdout_is_used_even_when_script_times_out_afterward() {
        let dir = unique_test_dir();
        fs::create_dir_all(&dir).unwrap();
        let script = dir.join("slow-after-json.sh");
        fs::write(
            &script,
            format!(
                "#!/usr/bin/env bash\ncat <<'JSON'\n{}\nJSON\nsleep 2\n",
                sample_json()
            ),
        )
        .unwrap();

        let result = run_ipv4_unlock_check_with_script(&script, Duration::from_millis(50))
            .await
            .unwrap();

        assert_eq!(result.items.len(), 7);
        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn successful_execution_parses_json_once() {
        let dir = unique_test_dir();
        fs::create_dir_all(&dir).unwrap();
        let script = dir.join("ipquality.sh");
        fs::write(
            &script,
            format!(
                "#!/usr/bin/env bash\ncat <<'JSON'\n{}\nJSON\n",
                sample_json()
            ),
        )
        .unwrap();

        let result = run_ipv4_unlock_check_with_script(&script, Duration::from_secs(1))
            .await
            .unwrap();

        assert_eq!(result.items.len(), 7);
        let _ = fs::remove_dir_all(dir);
    }

    fn unique_test_dir() -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("boil-unlock-check-{nanos}"))
    }
}
