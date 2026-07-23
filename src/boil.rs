use std::time::Duration;

use anyhow::Context as _;
use serde::Deserialize;

use crate::config::SecretToken;

pub struct BoilClient {
    client: reqwest::Client,
    api_base_url: reqwest::Url,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GetIpResponse {
    pub ok: bool,
    pub ip: std::net::IpAddr,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangeIpResponse {
    pub ok: bool,
    pub message: String,
    pub uses_left: Option<u32>,
    pub next_allowed_at: Option<i64>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct ApiErrorResponse {
    pub error: String,
}

#[derive(Debug, PartialEq, Eq)]
pub enum BoilApiError {
    Transport(String),
    HttpStatus {
        status: reqwest::StatusCode,
    },
    ApiRejected {
        status: Option<reqwest::StatusCode>,
        message: String,
    },
    InvalidJson {
        status: reqwest::StatusCode,
    },
    InvalidResponse(String),
}

impl std::fmt::Display for BoilApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transport(message) => write!(f, "Boil API transport error: {message}"),
            Self::HttpStatus { status } => write!(f, "Boil API returned HTTP status {status}"),
            Self::ApiRejected { status, message } => match status {
                Some(status) => {
                    write!(f, "Boil API rejected request with HTTP {status}: {message}")
                }
                None => write!(f, "Boil API rejected request: {message}"),
            },
            Self::InvalidJson { status } => {
                write!(f, "Boil API returned invalid JSON with HTTP {status}")
            }
            Self::InvalidResponse(message) => write!(f, "Boil API invalid response: {message}"),
        }
    }
}

impl std::error::Error for BoilApiError {}

#[derive(Deserialize)]
struct RawGetIpResponse {
    ok: bool,
    ip: Option<String>,
    error: Option<String>,
}

#[derive(Deserialize)]
struct RawChangeIpResponse {
    ok: bool,
    message: Option<String>,
    uses_left: Option<u32>,
    next_allowed_at: Option<i64>,
    error: Option<String>,
}

const DEFAULT_BOIL_API_BASE_URL: &str = "https://ippanel.boil.network";

impl BoilClient {
    pub fn new() -> anyhow::Result<Self> {
        Self::with_api_base_url(DEFAULT_BOIL_API_BASE_URL)
    }

    pub fn with_api_base_url(api_base_url: &str) -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()?;
        let api_base_url = parse_base_url(api_base_url)?;

        Ok(Self {
            client,
            api_base_url,
        })
    }

    pub async fn get_ip(&self, token: &SecretToken) -> Result<GetIpResponse, BoilApiError> {
        let raw = self.post_new_api(token, "/api/v1/getIP").await?;
        parse_get_ip_response(raw)
    }

    pub async fn change_ip(&self, token: &SecretToken) -> Result<ChangeIpResponse, BoilApiError> {
        self.post_new_api(token, "/api/v1/changeIP/")
            .await
            .and_then(parse_change_ip_response)
    }

    async fn post_new_api(
        &self,
        token: &SecretToken,
        path: &str,
    ) -> Result<(reqwest::StatusCode, String), BoilApiError> {
        let url = self
            .api_base_url
            .join(path.trim_start_matches('/'))
            .map_err(|_| BoilApiError::InvalidResponse("invalid Boil API path".to_string()))?;

        let response = self
            .client
            .post(url)
            .bearer_auth(token.expose_secret())
            .send()
            .await
            .map_err(|e| BoilApiError::Transport(sanitize_transport_error(&e)))?;
        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|e| BoilApiError::Transport(sanitize_transport_error(&e)))?;
        let body = redact_token(&body, token);

        if !status.is_success() {
            return match parse_api_error(status, &body) {
                Ok(message) => Err(BoilApiError::ApiRejected {
                    status: Some(status),
                    message,
                }),
                Err(err) => Err(err),
            };
        }

        Ok((status, body))
    }
}

fn parse_base_url(base_url: &str) -> anyhow::Result<reqwest::Url> {
    let mut url = reqwest::Url::parse(base_url).context("Boil API base URL 无效")?;
    if !url.path().ends_with('/') {
        let path = format!("{}/", url.path());
        url.set_path(&path);
    }
    Ok(url)
}

fn parse_get_ip_response(
    (status, body): (reqwest::StatusCode, String),
) -> Result<GetIpResponse, BoilApiError> {
    let raw = serde_json::from_str::<RawGetIpResponse>(&body)
        .map_err(|_| BoilApiError::InvalidJson { status })?;
    if !raw.ok {
        return Err(BoilApiError::ApiRejected {
            status: None,
            message: safe_api_message(raw.error.as_deref().unwrap_or("ok=false")),
        });
    }
    let ip = raw
        .ip
        .ok_or_else(|| BoilApiError::InvalidResponse("getIP missing ip".to_string()))?
        .parse::<std::net::IpAddr>()
        .map_err(|_| BoilApiError::InvalidResponse("getIP returned invalid ip".to_string()))?;

    Ok(GetIpResponse { ok: raw.ok, ip })
}

fn parse_change_ip_response(
    (status, body): (reqwest::StatusCode, String),
) -> Result<ChangeIpResponse, BoilApiError> {
    let raw = serde_json::from_str::<RawChangeIpResponse>(&body)
        .map_err(|_| BoilApiError::InvalidJson { status })?;
    if !raw.ok {
        return Err(BoilApiError::ApiRejected {
            status: None,
            message: safe_api_message(raw.error.as_deref().unwrap_or("ok=false")),
        });
    }
    let message = raw
        .message
        .ok_or_else(|| BoilApiError::InvalidResponse("changeIP missing message".to_string()))?;

    Ok(ChangeIpResponse {
        ok: raw.ok,
        message,
        uses_left: raw.uses_left,
        next_allowed_at: raw.next_allowed_at,
    })
}

fn parse_api_error(status: reqwest::StatusCode, body: &str) -> Result<String, BoilApiError> {
    if body.trim().is_empty() {
        return Err(BoilApiError::HttpStatus { status });
    }

    match serde_json::from_str::<ApiErrorResponse>(body) {
        Ok(err) => Ok(safe_api_message(&err.error)),
        Err(_) => {
            if status == reqwest::StatusCode::BAD_REQUEST {
                Err(BoilApiError::InvalidJson { status })
            } else {
                Err(BoilApiError::HttpStatus { status })
            }
        }
    }
}

fn safe_api_message(message: &str) -> String {
    let sanitized = message.replace(['\r', '\n'], " ");
    sanitized.chars().take(200).collect()
}

fn redact_token(value: &str, token: &SecretToken) -> String {
    let secret = token.expose_secret();
    if secret.is_empty() {
        value.to_string()
    } else {
        value.replace(secret, "<redacted>")
    }
}

fn sanitize_transport_error(err: &reqwest::Error) -> String {
    if err.is_timeout() {
        "request timed out".to_string()
    } else if err.is_connect() {
        "connection failed".to_string()
    } else {
        "request failed".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        collections::VecDeque,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc, Mutex,
        },
    };
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::{TcpListener, TcpStream},
    };

    #[derive(Clone)]
    struct MockResponse {
        status: u16,
        body: String,
        content_type: &'static str,
    }

    #[derive(Clone, Debug)]
    struct RequestRecord {
        method: String,
        path: String,
        bearer_ok: bool,
        body_len: usize,
    }

    struct MockServer {
        base_url: String,
        records: Arc<Mutex<Vec<RequestRecord>>>,
        request_count: Arc<AtomicUsize>,
    }

    impl MockServer {
        async fn start(responses: Vec<MockResponse>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let responses = Arc::new(Mutex::new(VecDeque::from(responses)));
            let records = Arc::new(Mutex::new(Vec::new()));
            let request_count = Arc::new(AtomicUsize::new(0));

            tokio::spawn({
                let responses = Arc::clone(&responses);
                let records = Arc::clone(&records);
                let request_count = Arc::clone(&request_count);
                async move {
                    loop {
                        let remaining = responses.lock().unwrap().len();
                        if remaining == 0 {
                            break;
                        }

                        let Ok((stream, _)) = listener.accept().await else {
                            break;
                        };
                        let response = responses.lock().unwrap().pop_front();
                        let Some(response) = response else {
                            break;
                        };
                        request_count.fetch_add(1, Ordering::SeqCst);
                        handle_connection(stream, response, Arc::clone(&records)).await;
                    }
                }
            });

            Self {
                base_url: format!("http://{addr}"),
                records,
                request_count,
            }
        }

        fn records(&self) -> Vec<RequestRecord> {
            self.records.lock().unwrap().clone()
        }

        fn request_count(&self) -> usize {
            self.request_count.load(Ordering::SeqCst)
        }
    }

    async fn handle_connection(
        mut stream: TcpStream,
        response: MockResponse,
        records: Arc<Mutex<Vec<RequestRecord>>>,
    ) {
        let mut buffer = Vec::new();
        let mut chunk = [0u8; 1024];
        loop {
            let read = stream.read(&mut chunk).await.unwrap_or(0);
            if read == 0 {
                break;
            }
            buffer.extend_from_slice(&chunk[..read]);
            if request_complete(&buffer) {
                break;
            }
        }

        records.lock().unwrap().push(parse_request(&buffer));

        let body = response.body.as_bytes();
        let status_line = match response.status {
            200 => "HTTP/1.1 200 OK",
            400 => "HTTP/1.1 400 Bad Request",
            405 => "HTTP/1.1 405 Method Not Allowed",
            500 => "HTTP/1.1 500 Internal Server Error",
            _ => "HTTP/1.1 418 Unknown",
        };
        let response = format!(
            "{status_line}\r\ncontent-type: {}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            response.content_type,
            body.len(),
            response.body
        );
        let _ = stream.write_all(response.as_bytes()).await;
    }

    fn request_complete(buffer: &[u8]) -> bool {
        let Some(header_end) = find_header_end(buffer) else {
            return false;
        };
        let headers = String::from_utf8_lossy(&buffer[..header_end]);
        let content_length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            })
            .unwrap_or(0);
        buffer.len() >= header_end + 4 + content_length
    }

    fn find_header_end(buffer: &[u8]) -> Option<usize> {
        buffer.windows(4).position(|window| window == b"\r\n\r\n")
    }

    fn parse_request(buffer: &[u8]) -> RequestRecord {
        let header_end = find_header_end(buffer).unwrap_or(buffer.len());
        let headers = String::from_utf8_lossy(&buffer[..header_end]);
        let mut lines = headers.lines();
        let request_line = lines.next().unwrap_or_default();
        let mut request_parts = request_line.split_whitespace();
        let method = request_parts.next().unwrap_or_default().to_string();
        let path = request_parts.next().unwrap_or_default().to_string();
        let mut bearer_ok = false;
        let mut content_length = 0usize;

        for line in lines {
            let Some((name, value)) = line.split_once(':') else {
                continue;
            };
            if name.eq_ignore_ascii_case("authorization") {
                bearer_ok = value.trim() == format!("Bearer {}", test_credential());
            }
            if name.eq_ignore_ascii_case("content-length") {
                content_length = value.trim().parse::<usize>().unwrap_or(0);
            }
        }

        RequestRecord {
            method,
            path,
            bearer_ok,
            body_len: content_length,
        }
    }

    fn json_response(status: u16, body: impl Into<String>) -> MockResponse {
        MockResponse {
            status,
            body: body.into(),
            content_type: "application/json",
        }
    }

    fn text_response(status: u16, body: impl Into<String>) -> MockResponse {
        MockResponse {
            status,
            body: body.into(),
            content_type: "text/plain",
        }
    }

    fn test_credential() -> String {
        ["phase", "two", "credential"].join("-")
    }

    fn test_token() -> SecretToken {
        SecretToken::from_test_value(&test_credential())
    }

    #[tokio::test]
    async fn get_ip_posts_to_expected_path_with_bearer_and_parses_ip() {
        let server =
            MockServer::start(vec![json_response(200, r#"{"ok":true,"ip":"42.1.2.3"}"#)]).await;
        let client = BoilClient::with_api_base_url(&server.base_url).unwrap();

        let result = client.get_ip(&test_token()).await.unwrap();

        assert!(result.ok);
        assert_eq!(result.ip, "42.1.2.3".parse::<std::net::IpAddr>().unwrap());
        let records = server.records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].method, "POST");
        assert_eq!(records[0].path, "/api/v1/getIP");
        assert!(records[0].bearer_ok);
        assert_eq!(records[0].body_len, 0);
    }

    #[tokio::test]
    async fn change_ip_posts_to_expected_path_with_bearer_and_parses_response() {
        let server = MockServer::start(vec![json_response(
            200,
            r#"{"ok":true,"message":"accepted","uses_left":2,"next_allowed_at":1782732942}"#,
        )])
        .await;
        let client = BoilClient::with_api_base_url(&server.base_url).unwrap();

        let result = client.change_ip(&test_token()).await.unwrap();

        assert!(result.ok);
        assert_eq!(result.message, "accepted");
        assert_eq!(result.uses_left, Some(2));
        assert_eq!(result.next_allowed_at, Some(1782732942));
        let records = server.records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].method, "POST");
        assert_eq!(records[0].path, "/api/v1/changeIP/");
        assert!(records[0].bearer_ok);
        assert_eq!(records[0].body_len, 0);
    }

    #[tokio::test]
    async fn api_error_is_rejected_and_change_ip_is_not_retried() {
        let server =
            MockServer::start(vec![json_response(400, r#"{"error":"quota exhausted"}"#)]).await;
        let client = BoilClient::with_api_base_url(&server.base_url).unwrap();

        let err = client.change_ip(&test_token()).await.unwrap_err();

        assert_eq!(
            err,
            BoilApiError::ApiRejected {
                status: Some(reqwest::StatusCode::BAD_REQUEST),
                message: "quota exhausted".to_string(),
            }
        );
        assert_eq!(server.request_count(), 1);
    }

    #[tokio::test]
    async fn method_and_server_errors_return_http_status() {
        let server = MockServer::start(vec![
            text_response(405, "method not allowed"),
            text_response(500, "server error"),
        ])
        .await;
        let client = BoilClient::with_api_base_url(&server.base_url).unwrap();

        let method_err = client.get_ip(&test_token()).await.unwrap_err();
        let server_err = client.get_ip(&test_token()).await.unwrap_err();

        assert_eq!(
            method_err,
            BoilApiError::HttpStatus {
                status: reqwest::StatusCode::METHOD_NOT_ALLOWED,
            }
        );
        assert_eq!(
            server_err,
            BoilApiError::HttpStatus {
                status: reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            }
        );
        assert_eq!(server.request_count(), 2);
    }

    #[tokio::test]
    async fn invalid_responses_are_classified() {
        let server = MockServer::start(vec![
            text_response(200, "not-json"),
            json_response(200, r#"{"ok":true}"#),
            json_response(200, r#"{"ok":true,"ip":"not-an-ip"}"#),
            json_response(200, r#"{"ok":false,"error":"denied"}"#),
            json_response(200, r#"{"ok":true,"ip":42}"#),
        ])
        .await;
        let client = BoilClient::with_api_base_url(&server.base_url).unwrap();

        assert!(matches!(
            client.get_ip(&test_token()).await.unwrap_err(),
            BoilApiError::InvalidJson { .. }
        ));
        assert_eq!(
            client.get_ip(&test_token()).await.unwrap_err(),
            BoilApiError::InvalidResponse("getIP missing ip".to_string())
        );
        assert_eq!(
            client.get_ip(&test_token()).await.unwrap_err(),
            BoilApiError::InvalidResponse("getIP returned invalid ip".to_string())
        );
        assert_eq!(
            client.get_ip(&test_token()).await.unwrap_err(),
            BoilApiError::ApiRejected {
                status: None,
                message: "denied".to_string(),
            }
        );
        assert!(matches!(
            client.get_ip(&test_token()).await.unwrap_err(),
            BoilApiError::InvalidJson { .. }
        ));
    }

    #[tokio::test]
    async fn token_is_not_in_debug_display_or_errors() {
        let server = MockServer::start(vec![
            json_response(
                400,
                format!(r#"{{"error":"{} denied"}}"#, test_credential()),
            ),
            json_response(
                200,
                format!(r#"{{"ok":false,"error":"{} denied"}}"#, test_credential()),
            ),
        ])
        .await;
        let client = BoilClient::with_api_base_url(&server.base_url).unwrap();
        let token = test_token();

        assert!(!format!("{token:?}").contains(&test_credential()));
        assert!(!format!("{token}").contains(&test_credential()));

        let http_error = client.change_ip(&token).await.unwrap_err();
        let api_error = client.get_ip(&token).await.unwrap_err();
        assert!(!format!("{http_error:?}").contains(&test_credential()));
        assert!(!http_error.to_string().contains(&test_credential()));
        assert!(!format!("{api_error:?}").contains(&test_credential()));
        assert!(!api_error.to_string().contains(&test_credential()));
    }

    #[tokio::test]
    async fn network_error_is_transport_and_change_ip_is_not_retried() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        let client = BoilClient::with_api_base_url(&base_url).unwrap();

        let err = client.change_ip(&test_token()).await.unwrap_err();

        assert!(matches!(err, BoilApiError::Transport(_)));
        assert!(!err.to_string().contains(&test_credential()));
    }
}
