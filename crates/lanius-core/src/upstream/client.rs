//! HTTP client for talking to the Kiro upstream API.
//!
//! [`KiroHttpClient`] wraps two `reqwest` clients (one for regular requests
//! with a total timeout, one for streaming requests with only a connect
//! timeout) and implements the retry/backoff/token-refresh policy used for
//! every upstream call: 403 triggers a forced token refresh, 429/5xx are
//! retried with exponential backoff, and transport-level failures are
//! retried only when [`crate::error::classify_network_error`] marks them
//! retryable. [`crate::server::AppState::initialize`] constructs a single
//! shared instance; [`crate::api`] route handlers call
//! [`KiroHttpClient::request_with_retry`] / [`KiroHttpClient::request_bytes_with_retry`]
//! to reach Kiro.

use std::sync::Arc;
use std::time::Duration;

use reqwest::{Method, Response, StatusCode};
use serde_json::Value;

use super::endpoint::{
    ChatEndpoint, EndpointThrottle, GLOBAL_THROTTLE, THROTTLE_DURATION, chat_endpoints,
};
use crate::auth::AuthManager;
use crate::config::{BASE_RETRY_DELAY, Config, MAX_RETRIES};
use crate::error::{GatewayError, Result, classify_network_error, enhance_kiro_error};
use crate::utils::kiro_headers;

#[derive(Clone, Copy)]
enum Target<'a> {
    Fixed(&'a str),
    Chat(&'a [ChatEndpoint], &'a EndpointThrottle),
}

// Total request timeout for non-streaming calls; streaming calls
// deliberately have no total timeout (see `client_timeouts`) since a
// legitimate long-running generation could otherwise be cut off.
const NON_STREAM_REQUEST_TIMEOUT: Duration = Duration::from_secs(300);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ClientTimeouts {
    request_timeout: Option<Duration>,
    connect_timeout: Duration,
}

// Streaming requests get no total request timeout — stalls are instead
// caught by the first-token/streaming-read timeouts applied at the
// stream-consumption layer (see `crate::upstream::stream`) — while
// non-streaming requests get a fixed total timeout. Both share the same
// connect timeout.
fn client_timeouts(stream: bool) -> ClientTimeouts {
    ClientTimeouts {
        request_timeout: (!stream).then_some(NON_STREAM_REQUEST_TIMEOUT),
        connect_timeout: CONNECT_TIMEOUT,
    }
}

/// HTTP client for the Kiro upstream API, encapsulating retry/backoff policy,
/// token refresh on auth failures, and the streaming-vs-non-streaming
/// timeout distinction. Cheap to hold behind an `Arc` and share across
/// requests (see [`crate::server::AppState::http_client`]).
pub struct KiroHttpClient {
    auth_manager: Arc<AuthManager>,
    client: reqwest::Client,
    streaming_client: reqwest::Client,
    first_token_max_retries: u32,
}

impl KiroHttpClient {
    /// Builds both the streaming and non-streaming `reqwest` clients (with
    /// an optional VPN/proxy applied to both) and wires up the auth manager
    /// used to fetch/refresh access tokens.
    ///
    /// # Examples
    ///
    /// ```
    /// use lanius_core::auth::AuthManager;
    /// use lanius_core::upstream::KiroHttpClient;
    /// use lanius_core::Config;
    /// use std::sync::Arc;
    ///
    /// let config = Config::default();
    /// let auth = Arc::new(AuthManager::new(config.clone()).expect("valid config"));
    /// let client = KiroHttpClient::new(auth, &config);
    /// assert!(client.is_ok());
    /// ```
    pub fn new(auth_manager: Arc<AuthManager>, config: &Config) -> Result<Self> {
        Ok(Self {
            auth_manager,
            client: build_client(config, client_timeouts(false))?,
            streaming_client: build_client(config, client_timeouts(true))?,
            first_token_max_retries: config.first_token_max_retries,
        })
    }

    /// Builds a client from an already-constructed `reqwest::Client`,
    /// reusing it for both streaming and non-streaming requests. Mainly
    /// useful in tests that need to inject a mocked/instrumented client.
    ///
    /// # Examples
    ///
    /// ```
    /// use lanius_core::auth::AuthManager;
    /// use lanius_core::upstream::KiroHttpClient;
    /// use lanius_core::Config;
    /// use std::sync::Arc;
    ///
    /// let auth = Arc::new(AuthManager::new(Config::default()).expect("valid config"));
    /// let client = KiroHttpClient::with_client(auth, reqwest::Client::new());
    /// ```
    pub fn with_client(auth_manager: Arc<AuthManager>, client: reqwest::Client) -> Self {
        Self {
            auth_manager,
            streaming_client: client.clone(),
            client,
            first_token_max_retries: MAX_RETRIES,
        }
    }

    // Streaming requests use the configurable `first_token_max_retries`
    // (since waiting for the first token is the client's primary retry
    // lever for flaky connections); non-streaming requests always use the
    // fixed `MAX_RETRIES` constant.
    fn max_retries(&self, stream: bool) -> u32 {
        if stream {
            self.first_token_max_retries
        } else {
            MAX_RETRIES
        }
    }

    /// Sends a JSON request to `url` with retry/backoff, serializing
    /// `json_data` (if any) as the request body. See
    /// [`request_bytes_with_retry`](Self::request_bytes_with_retry) for the
    /// full retry semantics.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use lanius_core::auth::AuthManager;
    /// use lanius_core::upstream::KiroHttpClient;
    /// use lanius_core::Config;
    /// use serde_json::json;
    /// use std::sync::Arc;
    ///
    /// # async fn example() {
    /// let config = Config::default();
    /// let auth = Arc::new(AuthManager::new(config.clone()).expect("valid config"));
    /// let client = KiroHttpClient::new(auth, &config).expect("client build");
    /// let response = client
    ///     .request_with_retry(reqwest::Method::POST, "https://example.com", Some(json!({})), None, false)
    ///     .await;
    /// # }
    /// ```
    pub async fn request_with_retry(
        &self,
        method: Method,
        url: &str,
        json_data: Option<Value>,
        params: Option<Vec<(String, String)>>,
        stream: bool,
    ) -> Result<Response> {
        let body = match json_data {
            Some(value) => Some(serde_json::to_vec(&value)?),
            None => None,
        };
        self.request_bytes_with_retry(method, url, body, params, stream)
            .await
    }

    /// Sends a `generateAssistantResponse` request, rotating across the chat
    /// endpoints from [`chat_endpoints`]. A `429` parks the endpoint that
    /// returned it (see [`GLOBAL_THROTTLE`]) and the request moves straight to
    /// the next free endpoint without consuming a retry; only when every
    /// endpoint is parked does it fall back to exponential backoff. Other
    /// statuses follow [`request_bytes_with_retry`](Self::request_bytes_with_retry).
    pub async fn chat_request_with_retry(&self, payload: &Value, stream: bool) -> Result<Response> {
        let region = self.auth_manager.region().await;
        let has_profile = self
            .auth_manager
            .profile_arn()
            .await
            .is_some_and(|arn| !arn.is_empty());
        let endpoints = chat_endpoints(&region, has_profile);
        self.chat_request_to(&endpoints, payload, stream, &GLOBAL_THROTTLE)
            .await
    }

    async fn chat_request_to(
        &self,
        endpoints: &[ChatEndpoint],
        payload: &Value,
        stream: bool,
        throttle: &EndpointThrottle,
    ) -> Result<Response> {
        let body = serde_json::to_vec(payload)?;
        self.send_with_retry(
            Method::POST,
            Target::Chat(endpoints, throttle),
            Some(body),
            None,
            stream,
        )
        .await
    }

    /// Sends a raw-body request to `url`, retrying up to
    /// [`max_retries`](Self::max_retries) times.
    ///
    /// Retry policy per attempt:
    /// - `200 OK` — returned immediately.
    /// - `403 Forbidden` — treated as a stale/invalid token: force a token
    ///   refresh via the auth manager and retry (no backoff delay, since the
    ///   fix is a fresh token, not waiting).
    /// - `429` or any `5xx` — retried after an exponential backoff delay
    ///   (see [`retry_delay`]); the last such response is remembered so it
    ///   can be surfaced if every attempt exhausts.
    /// - Any other status — returned immediately as a
    ///   [`GatewayError::Upstream`] built from the response body.
    /// - Transport-level errors — classified via
    ///   [`classify_network_error`]; non-retryable errors (or the last
    ///   attempt) return immediately, otherwise the same backoff delay is
    ///   applied before retrying.
    ///
    /// A fresh access token is fetched at the start of every attempt (not
    /// just after a 403), and streaming requests get an explicit
    /// `Connection: close` header to avoid connection reuse across
    /// long-lived streams.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use lanius_core::auth::AuthManager;
    /// use lanius_core::upstream::KiroHttpClient;
    /// use lanius_core::Config;
    /// use std::sync::Arc;
    ///
    /// # async fn example() {
    /// let config = Config::default();
    /// let auth = Arc::new(AuthManager::new(config.clone()).expect("valid config"));
    /// let client = KiroHttpClient::new(auth, &config).expect("client build");
    /// let response = client
    ///     .request_bytes_with_retry(reqwest::Method::GET, "https://example.com", None, None, false)
    ///     .await;
    /// # }
    /// ```
    pub async fn request_bytes_with_retry(
        &self,
        method: Method,
        url: &str,
        body: Option<Vec<u8>>,
        params: Option<Vec<(String, String)>>,
        stream: bool,
    ) -> Result<Response> {
        self.send_with_retry(method, Target::Fixed(url), body, params, stream)
            .await
    }

    async fn send_with_retry(
        &self,
        method: Method,
        target: Target<'_>,
        body: Option<Vec<u8>>,
        params: Option<Vec<(String, String)>>,
        stream: bool,
    ) -> Result<Response> {
        let mut last_network_error: Option<GatewayError> = None;
        let mut last_upstream: Option<(StatusCode, String)> = None;

        let max_retries = self.max_retries(stream);
        let mut attempt = 0;
        let mut free_switches = 0;
        while attempt < max_retries {
            // When every chat endpoint is parked, fall back to the preferred one;
            // the 429 branch below then backs off as the non-rotating path would.
            let chat_endpoint = match target {
                Target::Fixed(_) => None,
                Target::Chat(endpoints, throttle) => {
                    Some(&endpoints[throttle.pick(endpoints).unwrap_or(0)])
                }
            };
            let url = match (target, chat_endpoint) {
                (Target::Fixed(url), _) => url,
                (Target::Chat(..), endpoint) => endpoint.map_or("", |e| e.url.as_str()),
            };
            let token = self.auth_manager.access_token_and_autofetch().await?;
            let http_client = if stream {
                &self.streaming_client
            } else {
                &self.client
            };
            let target_url = append_query(url, params.as_deref())?;
            let mut headers = reqwest::header::HeaderMap::new();
            for (name, value) in kiro_headers(&token) {
                if let Ok(value) = reqwest::header::HeaderValue::from_str(&value) {
                    headers.insert(name, value);
                }
            }
            if let Some(endpoint) = chat_endpoint {
                headers.insert(
                    "x-amz-target",
                    reqwest::header::HeaderValue::from_static(endpoint.amz_target),
                );
            }
            if stream {
                headers.insert(
                    "Connection",
                    reqwest::header::HeaderValue::from_static("close"),
                );
            }
            let mut request = http_client.request(method.clone(), target_url);
            for (name, value) in &headers {
                request = request.header(name, value);
            }
            if let Some(body) = &body {
                request = request.body(body.clone());
            }

            match request.send().await {
                Ok(response) if response.status() == StatusCode::OK => return Ok(response),
                Ok(response) if response.status() == StatusCode::FORBIDDEN => {
                    let error_body = response.text().await.unwrap_or_default();
                    last_upstream = Some((StatusCode::FORBIDDEN, error_body));
                    tracing::warn!(
                        attempt = attempt + 1,
                        "upstream returned 403; forcing token refresh"
                    );
                    self.auth_manager.force_refresh().await?;
                }
                Ok(response)
                    if response.status() == StatusCode::TOO_MANY_REQUESTS
                        || response.status().is_server_error() =>
                {
                    let status = response.status();
                    let error_body = response.text().await.unwrap_or_default();
                    last_upstream = Some((status, error_body));
                    if let (Target::Chat(endpoints, throttle), Some(endpoint)) =
                        (target, chat_endpoint)
                    {
                        if status == StatusCode::TOO_MANY_REQUESTS {
                            throttle.throttle(endpoint.kind, THROTTLE_DURATION);
                            // Switching to a free endpoint is immediate and does
                            // not use up a retry; bounded by the endpoint count.
                            if free_switches < endpoints.len() && throttle.pick(endpoints).is_some()
                            {
                                free_switches += 1;
                                tracing::warn!(
                                    endpoint = endpoint.kind.as_str(),
                                    "chat endpoint rate-limited; switching endpoint"
                                );
                                continue;
                            }
                        }
                    }
                    tokio::time::sleep(retry_delay(attempt)).await;
                }
                Ok(response) => return Err(upstream_error(response).await),
                Err(error) => {
                    let info = classify_network_error(&error);
                    tracing::warn!(
                        attempt = attempt + 1,
                        category = %info.category,
                        details = %info.technical_details,
                        "upstream request failed"
                    );
                    let gateway_error = GatewayError::Network(Box::new(info.clone()));
                    last_network_error = Some(gateway_error);
                    if !info.is_retryable || attempt + 1 == max_retries {
                        return Err(last_network_error.unwrap_or_else(|| {
                            GatewayError::Internal(
                                "missing request error after failed request".to_string(),
                            )
                        }));
                    }
                    tokio::time::sleep(retry_delay(attempt)).await;
                }
            }
            attempt += 1;
        }

        if let Some((status, body)) = last_upstream {
            return Err(upstream_error_from_body(status, &body));
        }
        Err(last_network_error.unwrap_or_else(|| {
            GatewayError::Internal(
                "request retry loop completed without response or error".to_string(),
            )
        }))
    }

    /// Direct access to the non-streaming `reqwest::Client`, for callers
    /// (e.g. the `/usage` handler in [`crate::server`]) that need to issue a
    /// one-off request outside the retry wrapper above.
    ///
    /// # Examples
    ///
    /// ```
    /// use lanius_core::auth::AuthManager;
    /// use lanius_core::upstream::KiroHttpClient;
    /// use lanius_core::Config;
    /// use std::sync::Arc;
    ///
    /// let config = Config::default();
    /// let auth = Arc::new(AuthManager::new(config.clone()).expect("valid config"));
    /// let client = KiroHttpClient::new(auth, &config).expect("client build");
    /// let _reqwest_client = client.client();
    /// ```
    pub fn client(&self) -> &reqwest::Client {
        &self.client
    }
}

// Appends `params` as query-string pairs onto `url`, parsing/re-serializing
// through `reqwest::Url` so existing query parameters and encoding are
// preserved correctly. Returns `url` unchanged if there are no params.
fn append_query(url: &str, params: Option<&[(String, String)]>) -> Result<String> {
    let Some(params) = params.filter(|params| !params.is_empty()) else {
        return Ok(url.to_owned());
    };
    let mut parsed = reqwest::Url::parse(url)
        .map_err(|error| GatewayError::Config(format!("invalid upstream URL: {error}")))?;
    parsed
        .query_pairs_mut()
        .extend_pairs(params.iter().map(|(key, value)| (key, value)));
    Ok(parsed.to_string())
}

// Constructs a `reqwest::Client` with the given connect/request timeouts, a
// bounded redirect policy, and an optional VPN/proxy from config.
fn build_client(config: &Config, timeouts: ClientTimeouts) -> Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder()
        .connect_timeout(timeouts.connect_timeout)
        .redirect(reqwest::redirect::Policy::limited(10));
    if let Some(request_timeout) = timeouts.request_timeout {
        builder = builder.timeout(request_timeout);
    }
    if let Some(proxy_url) = config
        .vpn_proxy_url
        .as_deref()
        .filter(|url| !url.is_empty())
    {
        let proxy = reqwest::Proxy::all(proxy_url).map_err(|error| {
            GatewayError::Config(format!("invalid VPN_PROXY_URL {proxy_url:?}: {error}"))
        })?;
        builder = builder.proxy(proxy);
    }
    builder.build().map_err(|error| {
        GatewayError::Config(format!("failed to build upstream HTTP client: {error}"))
    })
}

// Exponential backoff: `BASE_RETRY_DELAY * 2^attempt` (1s, 2s, 4s, ...).
fn retry_delay(attempt: u32) -> Duration {
    BASE_RETRY_DELAY.saturating_mul(1_u32 << attempt)
}

// Reads the response body and builds a classified `GatewayError::Upstream`
// from it (see `upstream_error_from_body`).
async fn upstream_error(response: Response) -> GatewayError {
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    upstream_error_from_body(status, &body)
}

// Parses `body` as JSON (falling back to wrapping the raw text in a
// `{"message": ...}` object if it isn't valid JSON) and runs it through
// `enhance_kiro_error` to build a classified, user-facing error.
fn upstream_error_from_body(status: StatusCode, body: &str) -> GatewayError {
    let payload = serde_json::from_str::<Value>(body)
        .unwrap_or_else(|_| serde_json::json!({"message": body}));
    GatewayError::Upstream {
        status: status.as_u16(),
        info: Box::new(enhance_kiro_error(&payload)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::upstream::endpoint::ChatEndpointKind;

    #[test]
    fn streaming_uses_configured_first_token_retry_limit() {
        let client = KiroHttpClient {
            auth_manager: Arc::new(AuthManager::new(Config::default()).unwrap()),
            client: reqwest::Client::new(),
            streaming_client: reqwest::Client::new(),
            first_token_max_retries: 7,
        };
        assert_eq!(client.max_retries(true), 7);
        assert_eq!(client.max_retries(false), MAX_RETRIES);
    }

    #[test]
    fn streaming_client_has_no_total_timeout_but_keeps_connect_timeout() {
        assert_eq!(
            client_timeouts(true),
            ClientTimeouts {
                request_timeout: None,
                connect_timeout: CONNECT_TIMEOUT
            }
        );
        assert_eq!(
            client_timeouts(false),
            ClientTimeouts {
                request_timeout: Some(NON_STREAM_REQUEST_TIMEOUT),
                connect_timeout: CONNECT_TIMEOUT,
            }
        );
    }

    #[test]
    fn retry_delays_are_exponential() {
        assert_eq!(retry_delay(0), Duration::from_secs(1));
        assert_eq!(retry_delay(1), Duration::from_secs(2));
        assert_eq!(retry_delay(2), Duration::from_secs(4));
    }

    #[test]
    fn upstream_json_error_is_enhanced() {
        let error = upstream_error_from_body(
            StatusCode::BAD_REQUEST,
            r#"{"message":"Improperly formed request.","reason":"UNKNOWN"}"#,
        );
        match error {
            GatewayError::Upstream { status, info } => {
                assert_eq!(status, 400);
                assert!(info.user_message.contains("Kiro API rejected the request"));
            }
            other => panic!("expected upstream error, got {other:?}"),
        }
    }

    #[test]
    fn non_json_error_body_is_not_discarded() {
        let error = upstream_error_from_body(StatusCode::BAD_GATEWAY, "bad proxy");
        match error {
            GatewayError::Upstream { status, info } => {
                assert_eq!(status, 502);
                assert_eq!(info.original_message, "bad proxy");
            }
            other => panic!("expected upstream error, got {other:?}"),
        }
    }

    // Serves `responses.len()` HTTP requests, answering each with the next
    // status, and records the request paths plus their `x-amz-target`.
    async fn scripted_server(
        responses: Vec<&'static str>,
    ) -> (String, Arc<std::sync::Mutex<Vec<(String, String)>>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = seen.clone();
        tokio::spawn(async move {
            for status in responses {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let mut buf = vec![0_u8; 8192];
                let n = socket.read(&mut buf).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..n]).to_string();
                let path = request.split_whitespace().nth(1).unwrap_or("").to_string();
                let target = request
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .starts_with("x-amz-target:")
                            .then(|| line[13..].trim().to_string())
                    })
                    .unwrap_or_default();
                log.lock().unwrap().push((path, target));
                let reply = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok"
                );
                let _ = socket.write_all(reply.as_bytes()).await;
            }
        });
        (base, seen)
    }

    fn client_with_static_token() -> (KiroHttpClient, std::path::PathBuf) {
        let path = std::env::temp_dir().join(format!(
            "lanius-endpoint-test-{}.json",
            uuid::Uuid::new_v4()
        ));
        std::fs::write(
            &path,
            r#"{"accessToken":"t","refreshToken":"r","profileArn":"arn:test","expiresAt":"2099-01-01T00:00:00Z"}"#,
        )
        .unwrap();
        let config = Config {
            kiro_creds_file: Some(path.clone()),
            ..Config::default()
        };
        let auth = Arc::new(AuthManager::new(config).unwrap());
        (
            KiroHttpClient::with_client(auth, reqwest::Client::new()),
            path,
        )
    }

    fn endpoint(kind: ChatEndpointKind, base: &str, path: &str) -> ChatEndpoint {
        ChatEndpoint {
            kind,
            url: format!("{base}{path}"),
            amz_target: match kind {
                ChatEndpointKind::AmazonQ => "AmazonQDeveloperStreamingService.SendMessage",
                _ => "AmazonCodeWhispererStreamingService.GenerateAssistantResponse",
            },
        }
    }

    #[tokio::test]
    async fn chat_429_switches_endpoint_without_backoff_and_parks_the_first() {
        let (base, seen) = scripted_server(vec!["429 Too Many Requests", "200 OK"]).await;
        let (client, creds) = client_with_static_token();
        let endpoints = [
            endpoint(ChatEndpointKind::Runtime, &base, "/a"),
            endpoint(ChatEndpointKind::AmazonQ, &base, "/b"),
        ];
        let throttle = EndpointThrottle::default();

        let started = std::time::Instant::now();
        let response = client
            .chat_request_to(&endpoints, &serde_json::json!({}), true, &throttle)
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            started.elapsed() < BASE_RETRY_DELAY,
            "switching endpoints must not wait for backoff"
        );
        assert_eq!(
            *seen.lock().unwrap(),
            vec![
                (
                    "/a".to_string(),
                    "AmazonCodeWhispererStreamingService.GenerateAssistantResponse".to_string()
                ),
                (
                    "/b".to_string(),
                    "AmazonQDeveloperStreamingService.SendMessage".to_string()
                ),
            ]
        );
        assert!(throttle.is_throttled(ChatEndpointKind::Runtime));
        assert!(!throttle.is_throttled(ChatEndpointKind::AmazonQ));
        std::fs::remove_file(creds).unwrap();
    }

    #[tokio::test]
    async fn chat_request_starts_on_first_free_endpoint() {
        let (base, seen) = scripted_server(vec!["200 OK"]).await;
        let (client, creds) = client_with_static_token();
        let endpoints = [
            endpoint(ChatEndpointKind::Runtime, &base, "/a"),
            endpoint(ChatEndpointKind::Q, &base, "/b"),
        ];
        let throttle = EndpointThrottle::default();
        throttle.throttle(ChatEndpointKind::Runtime, THROTTLE_DURATION);

        client
            .chat_request_to(&endpoints, &serde_json::json!({}), true, &throttle)
            .await
            .unwrap();
        assert_eq!(seen.lock().unwrap()[0].0, "/b");
        std::fs::remove_file(creds).unwrap();
    }

    #[test]
    fn http_and_socks_proxies_build_without_network_io() {
        for proxy in ["http://127.0.0.1:8080", "socks5://127.0.0.1:1080"] {
            let config = Config {
                vpn_proxy_url: Some(proxy.to_string()),
                ..Config::default()
            };
            let auth = Arc::new(AuthManager::new(config.clone()).unwrap());
            assert!(KiroHttpClient::new(auth, &config).is_ok());
        }
    }
}
