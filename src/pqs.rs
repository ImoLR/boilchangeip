use std::{net::Ipv4Addr, time::Duration};

use crate::config::PqsConfig;

const DEFAULT_PQS_API_BASE_URL: &str = "https://api.pqs.pw/";

pub struct PqsClient {
    client: reqwest::Client,
    api_base_url: reqwest::Url,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PqsChangeAccepted;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PqsError {
    Transport,
    HttpStatus(u16),
    InvalidToken,
    RateLimited,
    InvalidPublicIpv4,
    InvalidResponse,
}

impl PqsError {
    pub fn is_definite_rejection(self) -> bool {
        matches!(self, Self::InvalidToken | Self::RateLimited)
    }
}

impl std::fmt::Display for PqsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::Transport => "PQS API 请求失败",
            Self::HttpStatus(status) => return write!(f, "PQS API 返回 HTTP {status}"),
            Self::InvalidToken => "PQS API Token 无效",
            Self::RateLimited => "PQS API 请求过于频繁",
            Self::InvalidPublicIpv4 => "PQS 查询接口没有返回有效公网 IPv4",
            Self::InvalidResponse => "PQS API 返回了无效响应",
        };
        f.write_str(message)
    }
}

impl std::error::Error for PqsError {}

impl PqsClient {
    pub fn new() -> anyhow::Result<Self> {
        Self::with_api_base_url(DEFAULT_PQS_API_BASE_URL)
    }

    pub fn with_api_base_url(api_base_url: &str) -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()?;
        let mut api_base_url = reqwest::Url::parse(api_base_url)?;
        if !api_base_url.path().ends_with('/') {
            let path = format!("{}/", api_base_url.path());
            api_base_url.set_path(&path);
        }
        Ok(Self {
            client,
            api_base_url,
        })
    }

    pub async fn get_ip(&self, config: &PqsConfig) -> Result<std::net::IpAddr, PqsError> {
        let (status, body) = self.get("show", config).await?;
        classify_common_error(status, &body)?;
        let ip = body
            .trim()
            .parse::<Ipv4Addr>()
            .map_err(|_| PqsError::InvalidPublicIpv4)?;
        if !is_public_ipv4(ip) {
            return Err(PqsError::InvalidPublicIpv4);
        }
        Ok(std::net::IpAddr::V4(ip))
    }

    pub async fn change_ip(&self, config: &PqsConfig) -> Result<PqsChangeAccepted, PqsError> {
        let (status, body) = self.get("ipch", config).await?;
        classify_common_error(status, &body)?;
        Ok(PqsChangeAccepted)
    }

    async fn get(
        &self,
        operation: &str,
        config: &PqsConfig,
    ) -> Result<(reqwest::StatusCode, String), PqsError> {
        let mut url = self
            .api_base_url
            .join(operation)
            .map_err(|_| PqsError::InvalidResponse)?;
        url.path_segments_mut()
            .map_err(|_| PqsError::InvalidResponse)?
            .push(config.token.expose_secret());

        let response = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|_| PqsError::Transport)?;
        let status = response.status();
        let body = response.text().await.map_err(|_| PqsError::Transport)?;
        Ok((status, body))
    }
}

fn classify_common_error(status: reqwest::StatusCode, body: &str) -> Result<(), PqsError> {
    let normalized = body.trim().to_ascii_lowercase();
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS
        || normalized.contains("rate limit")
        || normalized.contains("too many")
        || body.contains("頻繁")
        || body.contains("频繁")
    {
        return Err(PqsError::RateLimited);
    }
    if normalized.contains("invalid token") || normalized.contains("missing token") {
        return Err(PqsError::InvalidToken);
    }
    if !status.is_success() {
        return Err(PqsError::HttpStatus(status.as_u16()));
    }
    Ok(())
}

fn is_public_ipv4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !ip.is_private()
        && !ip.is_loopback()
        && !ip.is_link_local()
        && !ip.is_multicast()
        && !ip.is_unspecified()
        && ip != Ipv4Addr::BROADCAST
        && a != 0
        && !(a == 100 && (64..=127).contains(&b))
        && !(a == 192 && b == 0)
        && !(a == 198 && b == 51 && c == 100)
        && !(a == 203 && b == 0 && c == 113)
        && !(a == 198 && (b == 18 || b == 19))
        && a < 224
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SecretToken;
    use std::{
        collections::VecDeque,
        sync::{Arc, Mutex},
    };
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    struct MockServer {
        base_url: String,
        paths: Arc<Mutex<Vec<String>>>,
    }

    impl MockServer {
        async fn start(responses: Vec<(u16, &'static str)>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let responses = Arc::new(Mutex::new(VecDeque::from(responses)));
            let paths = Arc::new(Mutex::new(Vec::new()));
            tokio::spawn({
                let responses = Arc::clone(&responses);
                let paths = Arc::clone(&paths);
                async move {
                    loop {
                        let response = responses.lock().unwrap().pop_front();
                        let Some((status, body)) = response else {
                            break;
                        };
                        let (mut stream, _) = listener.accept().await.unwrap();
                        let mut bytes = Vec::new();
                        let mut chunk = [0_u8; 1024];
                        loop {
                            let read = stream.read(&mut chunk).await.unwrap();
                            if read == 0 {
                                break;
                            }
                            bytes.extend_from_slice(&chunk[..read]);
                            if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                                break;
                            }
                        }
                        let request = String::from_utf8_lossy(&bytes);
                        let path = request
                            .lines()
                            .next()
                            .and_then(|line| line.split_whitespace().nth(1))
                            .unwrap_or_default()
                            .to_string();
                        paths.lock().unwrap().push(path);
                        let status_text = match status {
                            200 => "200 OK",
                            429 => "429 Too Many Requests",
                            _ => "500 Internal Server Error",
                        };
                        let wire = format!(
                            "HTTP/1.1 {status_text}\r\ncontent-type: text/plain\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                            body.len()
                        );
                        stream.write_all(wire.as_bytes()).await.unwrap();
                    }
                }
            });
            Self {
                base_url: format!("http://{addr}"),
                paths,
            }
        }
    }

    fn config(token: &str) -> PqsConfig {
        PqsConfig {
            token: SecretToken::from_test_value(token),
            last_changeip_at: None,
            next_changeip_available_at: None,
            pending_old_ip: None,
            pending_attempts: None,
        }
    }

    #[tokio::test]
    async fn query_returns_only_valid_public_ipv4() {
        let mock = MockServer::start(vec![(200, "36.232.26.187\n")]).await;
        let client = PqsClient::with_api_base_url(&mock.base_url).unwrap();
        assert_eq!(
            client.get_ip(&config("hidden-token")).await.unwrap(),
            "36.232.26.187".parse::<std::net::IpAddr>().unwrap()
        );
        assert_eq!(
            mock.paths.lock().unwrap().as_slice(),
            ["/show/hidden-token"]
        );
    }

    #[tokio::test]
    async fn query_rejects_http_200_error_text_and_private_ip() {
        for body in ["Invalid token.", "192.168.1.1"] {
            let mock = MockServer::start(vec![(200, body)]).await;
            let client = PqsClient::with_api_base_url(&mock.base_url).unwrap();
            assert!(client.get_ip(&config("hidden-token")).await.is_err());
        }
    }

    #[tokio::test]
    async fn change_accepts_unstructured_success_without_exposing_token() {
        let mock = MockServer::start(vec![(200, "Success")]).await;
        let client = PqsClient::with_api_base_url(&mock.base_url).unwrap();
        client.change_ip(&config("hidden-token")).await.unwrap();
        assert_eq!(
            mock.paths.lock().unwrap().as_slice(),
            ["/ipch/hidden-token"]
        );

        let mock = MockServer::start(vec![(200, "Invalid token.")]).await;
        let client = PqsClient::with_api_base_url(&mock.base_url).unwrap();
        let error = client.change_ip(&config("super-secret")).await.unwrap_err();
        assert_eq!(error, PqsError::InvalidToken);
        assert!(!error.to_string().contains("super-secret"));
    }
}
