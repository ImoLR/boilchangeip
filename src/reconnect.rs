use std::{
    collections::HashMap,
    net::IpAddr,
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};

use crate::{
    boil::{BoilApiError, BoilClient},
    config::{AppConfig, ResolvedSelection, ServerConfig, ServerSelection},
};

static SERVER_LOCKS: OnceLock<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> =
    OnceLock::new();

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReconnectPolicy {
    pub initial_delay: Duration,
    pub poll_interval: Duration,
    pub max_poll_attempts: usize,
}

impl Default for ReconnectPolicy {
    fn default() -> Self {
        Self {
            initial_delay: Duration::from_secs(2),
            poll_interval: Duration::from_secs(2),
            max_poll_attempts: 3,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReconnectStatus {
    Success,
    Disabled,
    PreflightFailed,
    ApiRejected,
    ChangeAcceptedButUnconfirmed,
    InvalidResponse,
    NetworkError,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReconnectResult {
    pub server_id: String,
    pub server_name: String,
    pub old_ip: Option<IpAddr>,
    pub new_ip: Option<IpAddr>,
    pub changed: bool,
    pub uses_left: Option<u32>,
    pub next_allowed_at: Option<i64>,
    pub status: ReconnectStatus,
    pub message: Option<String>,
    pub poll_attempts: usize,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BatchReconnectResult {
    pub results: Vec<ReconnectResult>,
}

impl BatchReconnectResult {
    pub fn success_count(&self) -> usize {
        self.results
            .iter()
            .filter(|result| result.status == ReconnectStatus::Success)
            .count()
    }

    pub fn unconfirmed_count(&self) -> usize {
        self.results
            .iter()
            .filter(|result| result.status == ReconnectStatus::ChangeAcceptedButUnconfirmed)
            .count()
    }

    pub fn failure_count(&self) -> usize {
        self.results.len() - self.success_count() - self.unconfirmed_count()
    }
}

pub async fn reconnect_one(
    client: &BoilClient,
    server: &ServerConfig,
    policy: &ReconnectPolicy,
) -> ReconnectResult {
    if !server.enabled {
        return base_result(
            server,
            ReconnectStatus::Disabled,
            Some("server is disabled"),
        );
    }

    let lock = server_lock(&server.id);
    let _guard = lock.lock().await;
    reconnect_one_locked(client, server, policy).await
}

pub async fn reconnect_selected(
    client: &BoilClient,
    config: &AppConfig,
    selection: ServerSelection<'_>,
    policy: &ReconnectPolicy,
) -> anyhow::Result<BatchReconnectResult> {
    let selected = config.resolve_servers(selection)?;
    let mut results = Vec::new();

    match selected {
        ResolvedSelection::One(server) => {
            results.push(reconnect_one(client, server, policy).await);
        }
        ResolvedSelection::All(servers) => {
            for server in servers {
                results.push(reconnect_one(client, server, policy).await);
            }
        }
    }

    Ok(BatchReconnectResult { results })
}

async fn reconnect_one_locked(
    client: &BoilClient,
    server: &ServerConfig,
    policy: &ReconnectPolicy,
) -> ReconnectResult {
    let old_ip = match client.get_ip(&server.token).await {
        Ok(response) => response.ip,
        Err(error) => {
            let status = preflight_error_status(&error);
            return base_result(server, status, Some(&error.to_string()));
        }
    };

    let change = match client.change_ip(&server.token).await {
        Ok(response) => response,
        Err(error) => {
            let mut result = base_result(
                server,
                change_error_status(&error),
                Some(&error.to_string()),
            );
            result.old_ip = Some(old_ip);
            return result;
        }
    };

    let mut result = base_result(
        server,
        ReconnectStatus::ChangeAcceptedButUnconfirmed,
        Some(&change.message),
    );
    result.old_ip = Some(old_ip);
    result.uses_left = change.uses_left;
    result.next_allowed_at = change.next_allowed_at.filter(|timestamp| *timestamp >= 0);

    tokio::time::sleep(policy.initial_delay).await;

    for attempt in 1..=policy.max_poll_attempts {
        result.poll_attempts = attempt;
        match client.get_ip(&server.token).await {
            Ok(response) if response.ip != old_ip => {
                result.new_ip = Some(response.ip);
                result.changed = true;
                result.status = ReconnectStatus::Success;
                return result;
            }
            Ok(_) => {}
            Err(error) => {
                log::debug!(
                    "换 IP 后验证暂时失败: server_id={} attempt={attempt}: {}",
                    result.server_id,
                    redact_for_result(&error.to_string(), server)
                );
            }
        }

        if attempt < policy.max_poll_attempts {
            tokio::time::sleep(policy.poll_interval).await;
        }
    }

    if policy.max_poll_attempts == 0 {
        result.message = Some(redact_for_result(
            &format!("{}; no verification attempts configured", change.message),
            server,
        ));
    }
    result
}

fn base_result(
    server: &ServerConfig,
    status: ReconnectStatus,
    message: Option<&str>,
) -> ReconnectResult {
    ReconnectResult {
        server_id: redact_for_result(&server.id, server),
        server_name: redact_for_result(&server.name, server),
        old_ip: None,
        new_ip: None,
        changed: false,
        uses_left: None,
        next_allowed_at: None,
        status,
        message: message.map(|value| redact_for_result(value, server)),
        poll_attempts: 0,
    }
}

fn redact_for_result(value: &str, server: &ServerConfig) -> String {
    let token = server.token.expose_secret();
    if token.is_empty() {
        value.to_string()
    } else {
        value.replace(token, "<redacted>")
    }
}

fn preflight_error_status(error: &BoilApiError) -> ReconnectStatus {
    match error {
        BoilApiError::InvalidJson { .. } | BoilApiError::InvalidResponse(_) => {
            ReconnectStatus::InvalidResponse
        }
        BoilApiError::Transport(_) | BoilApiError::HttpStatus { .. } => {
            ReconnectStatus::PreflightFailed
        }
        BoilApiError::ApiRejected { .. } => ReconnectStatus::ApiRejected,
    }
}

fn change_error_status(error: &BoilApiError) -> ReconnectStatus {
    match error {
        BoilApiError::ApiRejected { .. } => ReconnectStatus::ApiRejected,
        BoilApiError::InvalidJson { .. } | BoilApiError::InvalidResponse(_) => {
            ReconnectStatus::InvalidResponse
        }
        BoilApiError::Transport(_) | BoilApiError::HttpStatus { .. } => {
            ReconnectStatus::NetworkError
        }
    }
}

fn server_lock(server_id: &str) -> Arc<tokio::sync::Mutex<()>> {
    let locks = SERVER_LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut locks = locks
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    Arc::clone(
        locks
            .entry(server_id.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SecretToken;
    use std::{
        collections::VecDeque,
        sync::atomic::{AtomicUsize, Ordering},
    };
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::{TcpListener, TcpStream},
    };

    #[derive(Clone)]
    struct MockResponse {
        status: u16,
        body: &'static str,
        delay: Duration,
        disconnect: bool,
    }

    struct MockServer {
        base_url: String,
        records: Arc<Mutex<Vec<String>>>,
        request_count: Arc<AtomicUsize>,
        max_active_changes: Arc<AtomicUsize>,
    }

    impl MockServer {
        async fn start(responses: Vec<MockResponse>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let responses = Arc::new(Mutex::new(VecDeque::from(responses)));
            let records = Arc::new(Mutex::new(Vec::new()));
            let request_count = Arc::new(AtomicUsize::new(0));
            let active_changes = Arc::new(AtomicUsize::new(0));
            let max_active_changes = Arc::new(AtomicUsize::new(0));

            tokio::spawn({
                let responses = Arc::clone(&responses);
                let records = Arc::clone(&records);
                let request_count = Arc::clone(&request_count);
                let active_changes = Arc::clone(&active_changes);
                let max_active_changes = Arc::clone(&max_active_changes);
                async move {
                    while let Ok((stream, _)) = listener.accept().await {
                        request_count.fetch_add(1, Ordering::SeqCst);
                        let response = responses.lock().unwrap().pop_front();
                        let Some(response) = response else { break };
                        tokio::spawn(handle_connection(
                            stream,
                            response,
                            Arc::clone(&records),
                            Arc::clone(&active_changes),
                            Arc::clone(&max_active_changes),
                        ));
                    }
                }
            });

            Self {
                base_url: format!("http://{addr}"),
                records,
                request_count,
                max_active_changes,
            }
        }

        fn records(&self) -> Vec<String> {
            self.records.lock().unwrap().clone()
        }

        fn change_count(&self) -> usize {
            self.records()
                .iter()
                .filter(|path| path.as_str() == "/api/v1/changeIP/")
                .count()
        }

        fn request_count(&self) -> usize {
            self.request_count.load(Ordering::SeqCst)
        }

        fn max_active_changes(&self) -> usize {
            self.max_active_changes.load(Ordering::SeqCst)
        }
    }

    async fn handle_connection(
        mut stream: TcpStream,
        response: MockResponse,
        records: Arc<Mutex<Vec<String>>>,
        active_changes: Arc<AtomicUsize>,
        max_active_changes: Arc<AtomicUsize>,
    ) {
        let mut buffer = Vec::new();
        let mut chunk = [0u8; 1024];
        loop {
            let read = stream.read(&mut chunk).await.unwrap_or(0);
            if read == 0 {
                return;
            }
            buffer.extend_from_slice(&chunk[..read]);
            if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }

        let request = String::from_utf8_lossy(&buffer);
        let path = request
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .unwrap_or_default()
            .to_string();
        records.lock().unwrap().push(path.clone());

        let is_change = path == "/api/v1/changeIP/";
        if is_change {
            let active = active_changes.fetch_add(1, Ordering::SeqCst) + 1;
            max_active_changes.fetch_max(active, Ordering::SeqCst);
        }
        tokio::time::sleep(response.delay).await;

        if response.disconnect {
            if is_change {
                active_changes.fetch_sub(1, Ordering::SeqCst);
            }
            return;
        }

        let status = match response.status {
            200 => "200 OK",
            400 => "400 Bad Request",
            500 => "500 Internal Server Error",
            _ => "418 Unknown",
        };
        let wire = format!(
            concat!(
                "HTTP/1.1 {}\r\n",
                "content-type: application/json\r\n",
                "content-length: {}\r\n",
                "connection: close\r\n\r\n{}"
            ),
            status,
            response.body.len(),
            response.body
        );
        let _ = stream.write_all(wire.as_bytes()).await;
        if is_change {
            active_changes.fetch_sub(1, Ordering::SeqCst);
        }
    }

    fn response(status: u16, body: &'static str) -> MockResponse {
        MockResponse {
            status,
            body,
            delay: Duration::ZERO,
            disconnect: false,
        }
    }

    fn delayed_response(status: u16, body: &'static str) -> MockResponse {
        MockResponse {
            status,
            body,
            delay: Duration::from_millis(30),
            disconnect: false,
        }
    }

    fn disconnect() -> MockResponse {
        MockResponse {
            status: 200,
            body: "",
            delay: Duration::ZERO,
            disconnect: true,
        }
    }

    fn ip(ip: &'static str) -> MockResponse {
        response(200, ip)
    }

    fn accepted() -> MockResponse {
        response(
            200,
            r#"{"ok":true,"message":"accepted","uses_left":2,"next_allowed_at":1782732942}"#,
        )
    }

    fn server(id: &str, enabled: bool) -> ServerConfig {
        ServerConfig {
            id: id.to_string(),
            name: format!("Server {id}"),
            token: SecretToken::from_test_value(&test_credential()),
            enabled,
            timer: None,
        }
    }

    fn config(servers: Vec<ServerConfig>) -> AppConfig {
        AppConfig {
            servers,
            tg_token: None,
            tg_chat_id: None,
            migration_notice: None,
        }
    }

    fn policy(attempts: usize) -> ReconnectPolicy {
        ReconnectPolicy {
            initial_delay: Duration::ZERO,
            poll_interval: Duration::ZERO,
            max_poll_attempts: attempts,
        }
    }

    fn test_credential() -> String {
        ["phase", "three", "credential"].join("-")
    }

    #[tokio::test]
    async fn reconnect_succeeds_with_one_change_request() {
        let mock = MockServer::start(vec![
            ip(r#"{"ok":true,"ip":"42.1.1.1"}"#),
            accepted(),
            ip(r#"{"ok":true,"ip":"42.1.1.2"}"#),
        ])
        .await;
        let client = BoilClient::with_api_base_url(&mock.base_url).unwrap();

        let result = reconnect_one(&client, &server("success", true), &policy(3)).await;

        assert_eq!(result.status, ReconnectStatus::Success);
        assert!(result.changed);
        assert_eq!(result.poll_attempts, 1);
        assert_eq!(result.uses_left, Some(2));
        assert_eq!(mock.change_count(), 1);
    }

    #[tokio::test]
    async fn reconnect_polls_without_repeating_change() {
        let mock = MockServer::start(vec![
            ip(r#"{"ok":true,"ip":"42.1.1.1"}"#),
            accepted(),
            ip(r#"{"ok":true,"ip":"42.1.1.1"}"#),
            ip(r#"{"ok":true,"ip":"42.1.1.1"}"#),
            ip(r#"{"ok":true,"ip":"42.1.1.2"}"#),
        ])
        .await;
        let client = BoilClient::with_api_base_url(&mock.base_url).unwrap();

        let result = reconnect_one(&client, &server("poll", true), &policy(3)).await;

        assert_eq!(result.status, ReconnectStatus::Success);
        assert_eq!(result.poll_attempts, 3);
        assert_eq!(mock.change_count(), 1);
    }

    #[tokio::test]
    async fn unchanged_ip_is_unconfirmed_without_second_change() {
        let mock = MockServer::start(vec![
            ip(r#"{"ok":true,"ip":"42.1.1.1"}"#),
            accepted(),
            ip(r#"{"ok":true,"ip":"42.1.1.1"}"#),
            ip(r#"{"ok":true,"ip":"42.1.1.1"}"#),
        ])
        .await;
        let client = BoilClient::with_api_base_url(&mock.base_url).unwrap();

        let result = reconnect_one(&client, &server("unchanged", true), &policy(2)).await;

        assert_eq!(result.status, ReconnectStatus::ChangeAcceptedButUnconfirmed);
        assert_eq!(result.poll_attempts, 2);
        assert_eq!(mock.change_count(), 1);
    }

    #[tokio::test]
    async fn transient_http_400_then_new_ip_succeeds_without_second_change() {
        let mock = MockServer::start(vec![
            ip(r#"{"ok":true,"ip":"42.1.1.1"}"#),
            accepted(),
            response(400, r#"{"error":"temporary backend error"}"#),
            ip(r#"{"ok":true,"ip":"42.1.1.2"}"#),
        ])
        .await;
        let client = BoilClient::with_api_base_url(&mock.base_url).unwrap();

        let result = reconnect_one(&client, &server("http-400", true), &policy(3)).await;

        assert_eq!(result.status, ReconnectStatus::Success);
        assert!(result.changed);
        assert_eq!(result.poll_attempts, 2);
        assert_eq!(mock.change_count(), 1);
    }

    #[tokio::test]
    async fn three_http_400_responses_are_unconfirmed_without_second_change() {
        let mock = MockServer::start(vec![
            ip(r#"{"ok":true,"ip":"42.1.1.1"}"#),
            accepted(),
            response(400, r#"{"error":"temporary backend error"}"#),
            response(400, r#"{"error":"temporary backend error"}"#),
            response(400, r#"{"error":"temporary backend error"}"#),
        ])
        .await;
        let client = BoilClient::with_api_base_url(&mock.base_url).unwrap();

        let result = reconnect_one(&client, &server("http-400", true), &policy(3)).await;

        assert_eq!(result.status, ReconnectStatus::ChangeAcceptedButUnconfirmed);
        assert_eq!(result.poll_attempts, 3);
        assert_eq!(mock.change_count(), 1);
        assert_eq!(mock.request_count(), 5);
        assert!(!result
            .message
            .as_deref()
            .unwrap_or_default()
            .contains("HTTP 400"));
    }

    #[tokio::test]
    async fn preflight_failure_never_calls_change() {
        let mock = MockServer::start(vec![response(500, "server unavailable")]).await;
        let client = BoilClient::with_api_base_url(&mock.base_url).unwrap();

        let result = reconnect_one(&client, &server("preflight", true), &policy(1)).await;

        assert_eq!(result.status, ReconnectStatus::PreflightFailed);
        assert_eq!(mock.change_count(), 0);
    }

    #[tokio::test]
    async fn rejected_change_is_not_retried_or_polled() {
        let mock = MockServer::start(vec![
            ip(r#"{"ok":true,"ip":"42.1.1.1"}"#),
            response(400, r#"{"error":"quota exhausted"}"#),
        ])
        .await;
        let client = BoilClient::with_api_base_url(&mock.base_url).unwrap();

        let result = reconnect_one(&client, &server("rejected", true), &policy(3)).await;

        assert_eq!(result.status, ReconnectStatus::ApiRejected);
        assert_eq!(result.poll_attempts, 0);
        assert_eq!(mock.change_count(), 1);
        assert_eq!(mock.records().len(), 2);
    }

    #[tokio::test]
    async fn accepted_change_with_invalid_poll_is_unconfirmed() {
        let mock = MockServer::start(vec![
            ip(r#"{"ok":true,"ip":"42.1.1.1"}"#),
            accepted(),
            response(200, "not-json"),
        ])
        .await;
        let client = BoilClient::with_api_base_url(&mock.base_url).unwrap();

        let result = reconnect_one(&client, &server("unconfirmed", true), &policy(3)).await;

        assert_eq!(result.status, ReconnectStatus::ChangeAcceptedButUnconfirmed);
        assert_eq!(result.poll_attempts, 3);
        assert_eq!(mock.change_count(), 1);
    }

    #[tokio::test]
    async fn accepted_change_with_network_failure_is_unconfirmed() {
        let mock = MockServer::start(vec![
            ip(r#"{"ok":true,"ip":"42.1.1.1"}"#),
            accepted(),
            disconnect(),
        ])
        .await;
        let client = BoilClient::with_api_base_url(&mock.base_url).unwrap();

        let result = reconnect_one(&client, &server("network", true), &policy(3)).await;

        assert_eq!(result.status, ReconnectStatus::ChangeAcceptedButUnconfirmed);
        assert_eq!(result.poll_attempts, 3);
        assert_eq!(mock.change_count(), 1);
    }

    #[tokio::test]
    async fn invalid_preflight_ip_has_explicit_status_and_no_change() {
        let mock = MockServer::start(vec![ip(r#"{"ok":true,"ip":"invalid"}"#)]).await;
        let client = BoilClient::with_api_base_url(&mock.base_url).unwrap();

        let result = reconnect_one(&client, &server("invalid", true), &policy(1)).await;

        assert_eq!(result.status, ReconnectStatus::InvalidResponse);
        assert_eq!(mock.change_count(), 0);
        assert!(!format!("{result:?}").contains(&test_credential()));
    }

    #[tokio::test]
    async fn disabled_server_makes_no_http_request() {
        let mock = MockServer::start(Vec::new()).await;
        let client = BoilClient::with_api_base_url(&mock.base_url).unwrap();

        let result = reconnect_one(&client, &server("disabled", false), &policy(1)).await;

        assert_eq!(result.status, ReconnectStatus::Disabled);
        assert_eq!(mock.request_count(), 0);
    }

    #[tokio::test]
    async fn unspecified_multiple_servers_fail_before_http() {
        let mock = MockServer::start(Vec::new()).await;
        let client = BoilClient::with_api_base_url(&mock.base_url).unwrap();
        let config = config(vec![server("a", true), server("b", true)]);

        let error = reconnect_selected(&client, &config, ServerSelection::Unspecified, &policy(1))
            .await
            .unwrap_err();

        assert!(error.to_string().contains("必须明确指定"));
        assert_eq!(mock.request_count(), 0);
    }

    #[tokio::test]
    async fn all_with_no_enabled_servers_fails_before_http() {
        let mock = MockServer::start(Vec::new()).await;
        let client = BoilClient::with_api_base_url(&mock.base_url).unwrap();
        let config = config(vec![
            server("disabled-a", false),
            server("disabled-b", false),
        ]);

        let error = reconnect_selected(&client, &config, ServerSelection::All, &policy(1))
            .await
            .unwrap_err();

        assert!(error.to_string().contains("没有已启用"));
        assert_eq!(mock.request_count(), 0);
    }

    #[tokio::test]
    async fn all_runs_in_configuration_order() {
        let mock = MockServer::start(vec![
            ip(r#"{"ok":true,"ip":"42.1.1.1"}"#),
            accepted(),
            ip(r#"{"ok":true,"ip":"42.1.1.2"}"#),
            ip(r#"{"ok":true,"ip":"42.2.2.1"}"#),
            accepted(),
            ip(r#"{"ok":true,"ip":"42.2.2.2"}"#),
        ])
        .await;
        let client = BoilClient::with_api_base_url(&mock.base_url).unwrap();
        let config = config(vec![
            server("a", true),
            server("ignored", false),
            server("b", true),
        ]);

        let batch = reconnect_selected(&client, &config, ServerSelection::All, &policy(1))
            .await
            .unwrap();

        assert_eq!(
            batch
                .results
                .iter()
                .map(|result| result.server_id.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "b"]
        );
        assert_eq!(batch.success_count(), 2);
        assert_eq!(
            mock.records(),
            vec![
                "/api/v1/getIP",
                "/api/v1/changeIP/",
                "/api/v1/getIP",
                "/api/v1/getIP",
                "/api/v1/changeIP/",
                "/api/v1/getIP",
            ]
        );
    }

    #[tokio::test]
    async fn batch_continues_after_partial_failure() {
        let mock = MockServer::start(vec![
            ip(r#"{"ok":true,"ip":"42.1.1.1"}"#),
            accepted(),
            ip(r#"{"ok":true,"ip":"42.1.1.2"}"#),
            ip(r#"{"ok":true,"ip":"42.2.2.1"}"#),
            response(400, r#"{"error":"denied"}"#),
            ip(r#"{"ok":true,"ip":"42.3.3.1"}"#),
            accepted(),
            ip(r#"{"ok":true,"ip":"42.3.3.2"}"#),
        ])
        .await;
        let client = BoilClient::with_api_base_url(&mock.base_url).unwrap();
        let config = config(vec![
            server("a", true),
            server("b", true),
            server("c", true),
        ]);

        let batch = reconnect_selected(&client, &config, ServerSelection::All, &policy(1))
            .await
            .unwrap();

        assert_eq!(batch.results.len(), 3);
        assert_eq!(batch.results[0].status, ReconnectStatus::Success);
        assert_eq!(batch.results[1].status, ReconnectStatus::ApiRejected);
        assert_eq!(batch.results[2].status, ReconnectStatus::Success);
        assert_eq!(batch.success_count(), 2);
        assert_eq!(batch.failure_count(), 1);
    }

    #[tokio::test]
    async fn same_server_reconnects_do_not_overlap_change_requests() {
        let mut delayed = accepted();
        delayed.delay = Duration::from_millis(30);
        let mock = MockServer::start(vec![
            ip(r#"{"ok":true,"ip":"42.1.1.1"}"#),
            delayed,
            ip(r#"{"ok":true,"ip":"42.1.1.2"}"#),
            ip(r#"{"ok":true,"ip":"42.1.1.2"}"#),
            delayed_response(200, r#"{"ok":true,"message":"accepted"}"#),
            ip(r#"{"ok":true,"ip":"42.1.1.3"}"#),
        ])
        .await;
        let client = Arc::new(BoilClient::with_api_base_url(&mock.base_url).unwrap());
        let server = Arc::new(server("concurrency", true));
        let policy = Arc::new(policy(1));

        let first = tokio::spawn({
            let client = Arc::clone(&client);
            let server = Arc::clone(&server);
            let policy = Arc::clone(&policy);
            async move { reconnect_one(&client, &server, &policy).await }
        });
        let second = tokio::spawn({
            let client = Arc::clone(&client);
            let server = Arc::clone(&server);
            let policy = Arc::clone(&policy);
            async move { reconnect_one(&client, &server, &policy).await }
        });

        let (first, second) = tokio::join!(first, second);
        assert_eq!(first.unwrap().status, ReconnectStatus::Success);
        assert_eq!(second.unwrap().status, ReconnectStatus::Success);
        assert_eq!(mock.change_count(), 2);
        assert_eq!(mock.max_active_changes(), 1);
    }

    #[tokio::test]
    async fn result_and_messages_do_not_expose_token() {
        let leaked: &'static str =
            Box::leak(format!(r#"{{"error":"{} denied"}}"#, test_credential()).into_boxed_str());
        let mock = MockServer::start(vec![
            ip(r#"{"ok":true,"ip":"42.1.1.1"}"#),
            response(400, leaked),
        ])
        .await;
        let client = BoilClient::with_api_base_url(&mock.base_url).unwrap();

        let result = reconnect_one(&client, &server("redacted", true), &policy(1)).await;
        let rendered = format!(
            "{result:?} {}",
            result.message.as_deref().unwrap_or_default()
        );

        assert!(!rendered.contains(&test_credential()));
        assert!(rendered.contains("<redacted>"));
    }

    #[tokio::test]
    async fn negative_next_allowed_timestamp_is_discarded() {
        let mock = MockServer::start(vec![
            ip(r#"{"ok":true,"ip":"42.1.1.1"}"#),
            response(
                200,
                r#"{"ok":true,"message":"accepted","next_allowed_at":-1}"#,
            ),
            ip(r#"{"ok":true,"ip":"42.1.1.2"}"#),
        ])
        .await;
        let client = BoilClient::with_api_base_url(&mock.base_url).unwrap();

        let result = reconnect_one(&client, &server("timestamp", true), &policy(1)).await;

        assert_eq!(result.status, ReconnectStatus::Success);
        assert_eq!(result.next_allowed_at, None);
    }
}
