//! `axum`-based HTTP server: route aggregation, CORS, and auth middleware.
//!
//! [`AppState`] is the shared application state constructed once at startup
//! (via `AppState::initialize`) and cloned cheaply into every request
//! handler. `app` assembles the final [`axum::Router`] by merging the
//! OpenAI-compatible routes ([`crate::api::openai_router`]) and the
//! Anthropic-compatible routes ([`crate::api::anthropic_router`]) with a
//! few gateway-level endpoints (`/usage`, `/account`), then layers on model-id
//! rewriting for Claude-style clients, local-only CORS, tracing, and panic
//! recovery. [`serve`] and [`spawn`] are the two public entry points used to
//! start the server (blocking vs. detached with a [`GatewayHandle`]).

use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, Method, Request, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use chrono::TimeZone;
use serde_json::{Value, json};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tower_http::catch_panic::CatchPanicLayer;
use tower_http::cors::{AllowHeaders, AllowMethods, AllowOrigin, CorsLayer};
use tower_http::trace::TraceLayer;

use crate::api::{AnthropicState, ErrorDetail, OpenAiState, anthropic_router, openai_router};
use crate::auth::AuthManager;
use crate::compat::{self, CompatibilityPipeline};
use crate::config::Config;
use crate::error::{GatewayError, Result};
use crate::model::{ModelInfoCache, ModelResolver};
use crate::truncation::TruncationStore;
use crate::upstream::KiroHttpClient;
use crate::utils::{API_RUNTIME, KIRO_ORIGIN, kiro_headers_for};

/// Shared application state handed to every route handler via `axum`'s
/// `State` extractor. Cloning is cheap: every field is an `Arc`, so all
/// clones share the same underlying config, auth manager, HTTP client,
/// caches, and truncation store.
#[derive(Clone)]
pub struct AppState {
    /// Effective gateway configuration.
    pub config: Arc<Config>,
    /// Handles token acquisition/refresh against Kiro/AWS.
    pub auth_manager: Arc<AuthManager>,
    /// Shared HTTP client used for all upstream Kiro requests.
    pub http_client: Arc<KiroHttpClient>,
    /// Cached model catalog (ids, context limits, reasoning capabilities).
    pub model_cache: Arc<ModelInfoCache>,
    /// Resolves client-supplied model names/aliases to Kiro model ids.
    pub model_resolver: Arc<ModelResolver>,
    /// Recovery records for upstream tool-call/content truncation.
    pub truncation_store: Arc<TruncationStore>,
    /// Per-client compatibility hooks (tool-name aliasing, model-id
    /// rewriting, etc.).
    pub compat: Arc<CompatibilityPipeline>,
}

impl AppState {
    // Builds the full application state for a single server instance:
    // validates required config, sets up the auth manager, seeds the model
    // cache with the built-in fallback list, and kicks off a background
    // task to refresh it from the live Kiro catalog (best-effort; failures
    // just keep the fallback list).
    async fn initialize(config: Config) -> Result<Self> {
        if config.proxy_api_key.trim().is_empty() || config.region.trim().is_empty() {
            return Err(GatewayError::Config(
                "proxy API key and region must not be empty".into(),
            ));
        }
        let config = Arc::new(config);
        let auth_manager = Arc::new(AuthManager::new((*config).clone())?);
        let http_client = Arc::new(KiroHttpClient::new(auth_manager.clone(), &config)?);

        let model_cache = Arc::new(ModelInfoCache::default());
        model_cache.load_fallback();
        {
            // Refresh the model catalog from Kiro in the background so
            // startup isn't blocked on a network round-trip; if it fails or
            // returns nothing we simply keep serving the fallback list.
            let cache = model_cache.clone();
            let auth = auth_manager.clone();
            let config = Arc::clone(&config);
            tokio::spawn(async move {
                match crate::model::fetch_available_models(auth, &config).await {
                    Some(models) if !models.is_empty() => {
                        let count = models.len();
                        cache.update(models);
                        tracing::info!(count, "refreshed model catalog from upstream");
                    }
                    _ => tracing::warn!(
                        "could not refresh model catalog; serving the built-in fallback list"
                    ),
                }
            });
        }
        let model_resolver = Arc::new(ModelResolver::from_config((*model_cache).clone(), &config));
        Ok(Self {
            compat: Arc::new(CompatibilityPipeline::new()),
            config,
            auth_manager,
            http_client,
            model_cache,
            model_resolver,
            truncation_store: Arc::new(TruncationStore::default()),
        })
    }

    // Builds the per-provider state slice for the OpenAI-compatible routes.
    fn openai_state(&self) -> OpenAiState {
        OpenAiState {
            config: (*self.config).clone(),
            auth_manager: self.auth_manager.clone(),
            model_cache: self.model_cache.clone(),
            model_resolver: (*self.model_resolver).clone(),
            truncation_store: (*self.truncation_store).clone(),
        }
    }

    // Builds the per-provider state slice for the Anthropic-compatible routes.
    fn anthropic_state(&self) -> AnthropicState {
        AnthropicState {
            config: self.config.clone(),
            auth_manager: self.auth_manager.clone(),
            model_cache: self.model_cache.clone(),
            truncation_store: (*self.truncation_store).clone(),
        }
    }
}

// Assembles the full router: merges the OpenAI and Anthropic route sets with
// the gateway-level `/usage` and `/account` endpoints, then layers on
// (innermost to outermost) model-id rewriting for Claude clients, local-only
// CORS, request tracing, and panic-to-500 recovery.
fn app(state: AppState) -> Router {
    let openai = openai_router().with_state(state.openai_state());
    let anthropic = anthropic_router().with_state(state.anthropic_state());
    Router::new()
        .merge(openai)
        .merge(anthropic)
        .route("/usage", get(usage))
        .route("/account", get(account))
        .with_state(state)
        .layer(middleware::from_fn(model_id_format_middleware))
        .layer(middleware::from_fn(failed_response_log_middleware))
        .layer(local_cors())
        // Failures are logged by `failed_response_log_middleware`; disable
        // tower-http's own 5xx ERROR line so each failure is logged once.
        .layer(TraceLayer::new_for_http().on_failure(()))
        .layer(CatchPanicLayer::new())
}

// Logs every non-success response with its method, path, status, latency
// and (when a handler attached one) the operator-facing `ErrorDetail`, so
// rejected or failed requests show up in the default `info` log output
// (tower-http's `TraceLayer` only logs requests at DEBUG and never logs 4xx).
// 5xx responses are logged at ERROR, 4xx at WARN, except 404 (unknown
// routes such as `/favicon.ico` probes), which is DEBUG noise. The detail
// extension is removed before the response leaves the server.
async fn failed_response_log_middleware(request: Request<Body>, next: Next) -> Response {
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let started = std::time::Instant::now();
    let mut response = next.run(request).await;
    let status = response.status();
    if !(status.is_client_error() || status.is_server_error()) {
        return response;
    }
    let latency_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let detail = match ErrorDetail::take(&mut response) {
        Some(detail) => detail,
        None => {
            // Responses built without an `ErrorDetail` (e.g. axum's own
            // extractor rejections) carry their reason in a small body;
            // buffer it so it can be logged, then hand it back intact.
            let (parts, body) = response.into_parts();
            let (text, body) = small_body_text(body).await;
            response = Response::from_parts(parts, body);
            text.or_else(|| status.canonical_reason().map(str::to_owned))
                .unwrap_or_default()
        }
    };
    let code = status.as_u16();
    if status.is_server_error() {
        tracing::error!(status = code, latency_ms, error = %detail, "{method} {path} failed");
    } else if status == StatusCode::NOT_FOUND {
        tracing::debug!(status = code, latency_ms, error = %detail, "{method} {path} rejected");
    } else {
        tracing::warn!(status = code, latency_ms, error = %detail, "{method} {path} rejected");
    }
    response
}

// Upper bound for buffering an error body just to log it.
const MAX_LOGGED_ERROR_BODY: u64 = 4096;

// Reads `body` into memory only when its exact length is known and small
// (never streaming bodies), returning its text for logging together with an
// equivalent body to send on. Anything else is passed through untouched.
async fn small_body_text(body: Body) -> (Option<String>, Body) {
    use axum::body::HttpBody as _;

    let Some(len) = body
        .size_hint()
        .exact()
        .filter(|len| (1..=MAX_LOGGED_ERROR_BODY).contains(len))
    else {
        return (None, body);
    };
    let limit = usize::try_from(len).unwrap_or(usize::MAX);
    match to_bytes(body, limit).await {
        Ok(bytes) => {
            let text = String::from_utf8_lossy(&bytes).trim().to_owned();
            ((!text.is_empty()).then_some(text), Body::from(bytes))
        }
        // Only reachable if an in-memory body of known size fails to read
        // or exceeds the size it advertised; the original body is consumed
        // by then, so the client gets an empty body with the same status.
        Err(_) => (None, Body::empty()),
    }
}

// CORS layer restricted to localhost/127.0.0.1 origins (any scheme/port),
// since the gateway is intended to be reached from local dev tools/browsers
// rather than arbitrary third-party origins.
fn local_cors() -> CorsLayer {
    CorsLayer::new()
        .allow_origin(AllowOrigin::predicate(|origin, _| is_local_origin(origin)))
        .allow_methods(AllowMethods::list([
            Method::GET,
            Method::POST,
            Method::OPTIONS,
        ]))
        .allow_headers(AllowHeaders::list([
            header::ACCEPT,
            header::AUTHORIZATION,
            header::CONTENT_TYPE,
            axum::http::HeaderName::from_static("x-api-key"),
            axum::http::HeaderName::from_static("anthropic-version"),
        ]))
}

// Validates that an `Origin` header is `http(s)://localhost[:port]` or
// `http(s)://127.0.0.1[:port]` with no path/query/userinfo component, used
// as the CORS allow-origin predicate above.
fn is_local_origin(origin: &HeaderValue) -> bool {
    let Ok(origin) = origin.to_str() else {
        return false;
    };
    let Some((scheme, authority)) = origin.split_once("://") else {
        return false;
    };
    if !matches!(scheme, "http" | "https") || authority.contains(['/', '?', '#', '@']) {
        return false;
    }
    let (host, port) = authority
        .split_once(':')
        .map_or((authority, None), |(host, port)| (host, Some(port)));
    matches!(host, "localhost" | "127.0.0.1")
        && port
            .is_none_or(|port| !port.is_empty() && port.bytes().all(|byte| byte.is_ascii_digit()))
}

// Middleware that rewrites model ids in `GET /v1/models` responses for
// Claude-style clients (detected via `compat::is_claude_client`), so their
// expected model-id format is preserved without affecting other clients.
// Buffers the whole response body to rewrite it, then recomputes
// Content-Length/Content-Type; only runs for the exact matching
// method+path+client combination and a 200 response.
async fn model_id_format_middleware(request: Request<Body>, next: Next) -> Response {
    let should_rewrite = request.method() == axum::http::Method::GET
        && request.uri().path() == "/v1/models"
        && compat::is_claude_client(request.headers());
    let response = next.run(request).await;
    if !should_rewrite || response.status() != StatusCode::OK {
        return response;
    }
    let (mut parts, body) = response.into_parts();
    let bytes = match to_bytes(body, usize::MAX).await {
        Ok(bytes) => bytes,
        Err(_) => return Response::from_parts(parts, Body::empty()),
    };
    let rewritten = compat::rewrite_model_ids(&bytes);
    parts.headers.remove(header::CONTENT_LENGTH);
    parts.headers.remove(header::CONTENT_TYPE);
    parts.headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    if let Ok(value) = HeaderValue::from_str(&rewritten.len().to_string()) {
        parts.headers.insert(header::CONTENT_LENGTH, value);
    }
    Response::from_parts(parts, Body::from(rewritten))
}

// `GET /usage`: proxies Kiro's raw usage/quota payload, gated by the
// shared proxy API key.
async fn usage(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if !authorized(&headers, &state.config.proxy_api_key) {
        return unauthorized();
    }
    match fetch_usage(&state).await {
        Ok(value) => Json(value).into_response(),
        Err(response) => *response,
    }
}

// `GET /account`: like `usage`, but returns a normalized summary shaped by
// `account_summary` instead of Kiro's raw payload.
async fn account(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if !authorized(&headers, &state.config.proxy_api_key) {
        return unauthorized();
    }
    match fetch_usage(&state).await {
        Ok(value) => Json(account_summary(&value)).into_response(),
        Err(response) => *response,
    }
}

// Fetches raw usage-limits data from Kiro's control plane using the
// configured account's auth. Returns `Err` with an already-built client
// `Response` (502/etc.) on any failure so callers can short-circuit
// without duplicating error-to-response mapping.
async fn fetch_usage(state: &AppState) -> std::result::Result<Value, Box<Response>> {
    let auth = &state.auth_manager;
    let token = auth
        .access_token_and_autofetch()
        .await
        .map_err(|error| Box::new(gateway_response(&error, "Failed to fetch usage")))?;
    let mut payload = json!({"origin":KIRO_ORIGIN,"isEmailRequired":true});
    // The Kiro control plane requires the profile ARN for CLI-identified requests.
    if let Some(profile_arn) = auth.profile_arn().await.filter(|arn| !arn.is_empty()) {
        payload["profileArn"] = Value::String(profile_arn);
    }
    let body = serde_json::to_vec(&payload)
        .map_err(|error| Box::new(gateway_response(&error.into(), "Failed to fetch usage")))?;
    let mut headers = reqwest::header::HeaderMap::new();
    for (name, value) in kiro_headers_for(&token, API_RUNTIME) {
        if let Ok(value) = HeaderValue::from_str(&value) {
            headers.insert(name, value);
        }
    }
    headers.insert(
        "x-amz-target",
        HeaderValue::from_static(
            "com.amazon.aws.codewhisperer.runtime.AmazonCodeWhispererService.GetUsageLimits",
        ),
    );
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/x-amz-json-1.0"),
    );
    let url = auth.control_plane_host().await;
    let mut request = state.http_client.client().post(url).body(body);
    for (name, value) in &headers {
        request = request.header(name, value);
    }
    let response = request
        .send()
        .await
        .map_err(|error| Box::new(gateway_response(&error.into(), "Failed to fetch usage")))?;
    let status = response.status();
    let bytes = response
        .bytes()
        .await
        .map_err(|error| Box::new(gateway_response(&error.into(), "Failed to fetch usage")))?;
    if status != reqwest::StatusCode::OK {
        return Err(Box::new(upstream_usage_error_response(status, &bytes)));
    }
    serde_json::from_slice(&bytes).map_err(|_| {
        Box::new(
            (
                StatusCode::BAD_GATEWAY,
                Json(json!({"detail":"Failed to fetch usage: invalid JSON response"})),
            )
                .into_response(),
        )
    })
}

// Builds a generic error response for a failed control-plane usage request
// without leaking the upstream response body (which may contain sensitive
// account details) to the client; only the status and body length are logged.
fn upstream_usage_error_response(status: reqwest::StatusCode, bytes: &[u8]) -> Response {
    tracing::debug!(
        upstream_status = %status,
        upstream_body_bytes = bytes.len(),
        upstream_body = "[redacted]",
        "control-plane usage request returned non-success status"
    );
    (
        StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
        Json(json!({"detail":"Failed to fetch usage from upstream"})),
    )
        .into_response()
}

// Converts a `GatewayError` into an HTTP `Response`, prefixing the message
// with additional context for network/timeout errors (whose default message
// alone may not make clear which operation failed).
fn gateway_response(error: &GatewayError, prefix: &str) -> Response {
    let status = StatusCode::from_u16(error.http_status()).unwrap_or(StatusCode::BAD_GATEWAY);
    let detail = if matches!(
        *error,
        GatewayError::Network(_)
            | GatewayError::FirstTokenTimeout(_)
            | GatewayError::StreamReadTimeout(_)
    ) {
        format!("{prefix}: {}", error.user_message())
    } else {
        error.user_message()
    };
    (status, Json(json!({"detail":detail}))).into_response()
}

// Reshapes Kiro's raw usage-limits JSON into a flatter, UI-friendly account
// summary: combines free-trial and standard quota/usage, sums any active
// bonus allotments, and preserves "falsy-is-absent" semantics (0 quota/usage
// is treated the same as missing).
fn account_summary(usage: &Value) -> Value {
    let user = usage.get("userInfo").and_then(Value::as_object);
    let subscription = usage.get("subscriptionInfo").and_then(Value::as_object);
    let breakdown = usage
        .get("usageBreakdownList")
        .and_then(Value::as_array)
        .and_then(|items| items.first())
        .and_then(Value::as_object);
    let empty = serde_json::Map::new();
    let user = user.unwrap_or(&empty);
    let subscription = subscription.unwrap_or(&empty);
    let breakdown = breakdown.unwrap_or(&empty);
    let trial = breakdown
        .get("freeTrialInfo")
        .and_then(Value::as_object)
        .unwrap_or(&empty);
    let is_trial = trial.get("freeTrialStatus").and_then(Value::as_str) == Some("ACTIVE");
    let trial_quota = precision_or_regular(trial, "usageLimitWithPrecision", "usageLimit");
    let trial_usage = precision_or_regular(trial, "currentUsageWithPrecision", "currentUsage");
    let free_quota = precision_or_regular(breakdown, "usageLimitWithPrecision", "usageLimit");
    let free_usage = precision_or_regular(breakdown, "currentUsageWithPrecision", "currentUsage");
    let bonuses: Vec<&serde_json::Map<String, Value>> = breakdown
        .get("bonuses")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_object)
                .filter(|bonus| bonus.get("status").and_then(Value::as_str) == Some("ACTIVE"))
                .collect()
        })
        .unwrap_or_default();
    let bonus_quota = (!bonuses.is_empty()).then(|| {
        bonuses
            .iter()
            .map(|bonus| precision_or_regular(bonus, "usageLimitWithPrecision", "usageLimit"))
            .sum::<f64>()
    });
    let bonus_usage = (!bonuses.is_empty()).then(|| {
        bonuses
            .iter()
            .map(|bonus| precision_or_regular(bonus, "currentUsageWithPrecision", "currentUsage"))
            .sum::<f64>()
    });
    let total_quota = trial_quota + free_quota;
    let total_usage = trial_usage + free_usage;
    let trial_expiry = trial
        .get("freeTrialExpiry")
        .and_then(Value::as_i64)
        .and_then(|seconds| chrono::Local.timestamp_opt(seconds, 0).single())
        .map(|time| time.format("%Y-%m-%d").to_string());
    let account_status = subscription
        .get("status")
        .cloned()
        .unwrap_or_else(|| Value::String(if is_trial { "Trial" } else { "Active" }.into()));
    json!({
        "accountName": user.get("email").cloned().unwrap_or_else(|| Value::String("User".into())), "email": user.get("email").cloned().unwrap_or(Value::Null), "provider": user.get("provider").cloned().unwrap_or(Value::Null),
        "planType": subscription.get("type").cloned().unwrap_or_else(|| Value::String("Free".into())), "subscriptionTitle": subscription.get("subscriptionTitle").cloned().unwrap_or(Value::Null), "isTrial": is_trial,
        "trialExpiryDate": trial_expiry, "subscriptionExpiryDate": subscription.get("expiryDate").cloned().or_else(|| subscription.get("subscriptionExpiryDate").cloned()).unwrap_or(Value::Null),
        "totalQuota": total_quota, "currentUsage": total_usage, "remainingQuota": (total_quota != 0.0 && total_usage != 0.0).then_some(total_quota-total_usage), "usagePercentage": if total_quota != 0.0 && total_usage != 0.0 { ((total_usage / total_quota * 100.0) * 100.0).round() / 100.0 } else { 0.0 },
        "trialQuota": trial_quota, "trialUsage": trial_usage, "freeQuota": free_quota, "freeUsage": free_usage,
        "bonusQuota": bonus_quota, "bonusUsage": bonus_usage, "bonusRemaining": bonus_quota.zip(bonus_usage).and_then(|(quota, used)| (quota != 0.0 && used != 0.0).then_some(quota-used)),
        "accountStatus": account_status, "resetDate": breakdown.get("resetDate").cloned().unwrap_or(Value::Null), "overageEnabled": breakdown.get("overageEnabled").cloned().unwrap_or(Value::Bool(false)),
    })
}

// Prefers a "with precision" numeric field over its rounded counterpart,
// falling back to the rounded field when the precise one is absent or zero
// (Kiro sometimes omits precision fields, represented as 0.0 here).
fn precision_or_regular(
    values: &serde_json::Map<String, Value>,
    precision: &str,
    regular: &str,
) -> f64 {
    values
        .get(precision)
        .and_then(Value::as_f64)
        .filter(|value| *value != 0.0)
        .or_else(|| values.get(regular).and_then(Value::as_f64))
        .unwrap_or(0.0)
}

// Checks the `Authorization: Bearer <key>` header against the configured
// proxy API key using a constant-time comparison (XOR-accumulate over every
// byte position) to avoid leaking key length/content via timing side
// channels.
fn authorized(headers: &HeaderMap, key: &str) -> bool {
    let supplied = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    let expected = format!("Bearer {key}");
    let mut difference = supplied.len() ^ expected.len();
    for (index, byte) in expected.bytes().enumerate() {
        difference |=
            usize::from(byte ^ supplied.as_bytes().get(index).copied().unwrap_or_default());
    }
    difference == 0
}
// Standard 401 response body for a missing/invalid proxy API key.
fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({"detail":"Invalid or missing API Key"})),
    )
        .into_response()
}

/// Binds to the configured host/port and serves the gateway until `shutdown`
/// resolves. This call blocks for the lifetime of the server; use [`spawn`]
/// instead if you need a handle to control shutdown externally (e.g. from
/// tests or an embedding application).
///
/// # Examples
///
/// ```no_run
/// use lanius_core::server::serve;
/// use lanius_core::Config;
///
/// # async fn example() {
/// let config = Config::default();
/// let shutdown = async { tokio::signal::ctrl_c().await.ok(); };
/// serve(config, shutdown).await.expect("server failed");
/// # }
/// ```
pub async fn serve(
    config: Config,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<()> {
    let address: SocketAddr = format!("{}:{}", config.server_host, config.server_port)
        .parse()
        .map_err(|error| GatewayError::Config(format!("invalid server bind address: {error}")))?;
    let listener = tokio::net::TcpListener::bind(address).await?;
    run(listener, config, shutdown).await
}

/// Handle to a gateway server started with [`spawn`], allowing the caller to
/// discover the bound address and later trigger a graceful shutdown.
pub struct GatewayHandle {
    local_addr: SocketAddr,
    shutdown: Option<oneshot::Sender<()>>,
    join: JoinHandle<Result<()>>,
}
impl GatewayHandle {
    /// The socket address the server actually bound to (useful when the
    /// configured port was `0` and an ephemeral port was assigned).
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use lanius_core::server::spawn;
    /// use lanius_core::Config;
    ///
    /// # async fn example() {
    /// let config = Config { server_port: 0, ..Config::default() };
    /// let handle = spawn(config).await.expect("spawn failed");
    /// println!("listening on {}", handle.local_addr());
    /// # }
    /// ```
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }
    /// Signals the server to begin a graceful shutdown and awaits its
    /// background task to finish. Safe to call at most once per handle
    /// (subsequent calls are no-ops for the shutdown signal, since the
    /// sender is consumed).
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use lanius_core::server::spawn;
    /// use lanius_core::Config;
    ///
    /// # async fn example() {
    /// let config = Config { server_port: 0, ..Config::default() };
    /// let handle = spawn(config).await.expect("spawn failed");
    /// handle.shutdown().await.expect("shutdown failed");
    /// # }
    /// ```
    pub async fn shutdown(mut self) -> Result<()> {
        if let Some(sender) = self.shutdown.take() {
            let _ = sender.send(());
        }
        self.join
            .await
            .map_err(|error| GatewayError::Internal(format!("gateway task join failed: {error}")))?
    }
}

/// Binds to the configured host/port and starts serving the gateway on a
/// background `tokio` task, returning immediately with a [`GatewayHandle`]
/// that can later be used to shut it down gracefully. Prefer this over
/// [`serve`] when the caller needs to keep control of its own task (e.g.
/// integration tests spawning an ephemeral server).
///
/// # Examples
///
/// ```no_run
/// use lanius_core::server::spawn;
/// use lanius_core::Config;
///
/// # async fn example() {
/// // A port of 0 lets the OS assign an ephemeral port, useful for tests.
/// let config = Config { server_port: 0, ..Config::default() };
/// let handle = spawn(config).await.expect("spawn failed");
/// handle.shutdown().await.expect("shutdown failed");
/// # }
/// ```
pub async fn spawn(config: Config) -> Result<GatewayHandle> {
    let address: SocketAddr = format!("{}:{}", config.server_host, config.server_port)
        .parse()
        .map_err(|error| GatewayError::Config(format!("invalid server bind address: {error}")))?;
    let listener = tokio::net::TcpListener::bind(address).await?;
    let local_addr = listener.local_addr()?;
    let (sender, receiver) = oneshot::channel();
    let join = tokio::spawn(async move {
        run(listener, config, async move {
            let _ = receiver.await;
        })
        .await
    });
    Ok(GatewayHandle {
        local_addr,
        shutdown: Some(sender),
        join,
    })
}

// Shared implementation behind `serve`/`spawn`: initializes `AppState` and
// serves the router with graceful shutdown.
async fn run(
    listener: tokio::net::TcpListener,
    config: Config,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<()> {
    let state = AppState::initialize(config).await?;
    let server = axum::serve(listener, app(state.clone())).with_graceful_shutdown(shutdown);
    server.await.map_err(GatewayError::Io)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use tower::ServiceExt;

    #[tokio::test]
    async fn spawn_binds_ephemeral_port_and_serves_health() {
        let config = Config {
            server_host: "127.0.0.1".into(),
            server_port: 0,
            ..Config::default()
        };
        let handle = spawn(config)
            .await
            .unwrap_or_else(|error| panic!("spawn failed: {error}"));
        let response = reqwest::get(format!("http://{}/health", handle.local_addr()))
            .await
            .unwrap_or_else(|error| panic!("health request failed: {error}"));
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        handle
            .shutdown()
            .await
            .unwrap_or_else(|error| panic!("shutdown failed: {error}"));
    }

    #[tokio::test]
    async fn catch_panic_layer_turns_handler_panic_into_500() {
        let router = Router::new()
            .route(
                "/panic",
                get(|| async {
                    panic!("test panic");
                    #[allow(unreachable_code)]
                    "unreachable"
                }),
            )
            .layer(CatchPanicLayer::new());
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/panic")
                    .body(Body::empty())
                    .unwrap_or_else(|error| panic!("request build failed: {error}")),
            )
            .await
            .unwrap_or_else(|never| match never {});
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn account_route_keeps_falsy_is_absent_semantics_and_all_fields() {
        let at = chrono::Local
            .with_ymd_and_hms(2026, 1, 2, 0, 0, 0)
            .single()
            .unwrap_or_else(|| panic!("fixed local time must exist"));
        let value = account_summary(
            &json!({"userInfo":{"email":"a@b","provider":"x"},"subscriptionInfo":{"type":"Pro"},"usageBreakdownList":[{"freeTrialInfo":{"freeTrialStatus":"ACTIVE","freeTrialExpiry":at.timestamp(),"usageLimit":5,"currentUsage":2},"usageLimit":0,"currentUsage":0,"bonuses":[{"status":"ACTIVE","usageLimit":3,"currentUsage":1}]}]}),
        );
        assert_eq!(value.as_object().map(serde_json::Map::len), Some(22));
        assert_eq!(value["remainingQuota"], 3.0);
        assert_eq!(value["bonusRemaining"], 2.0);
    }

    #[tokio::test]
    async fn control_plane_error_body_is_not_returned_to_clients() {
        let secret = b"Authorization: Bearer upstream-secret-token";
        let response = upstream_usage_error_response(reqwest::StatusCode::BAD_GATEWAY, secret);
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap_or_else(|error| panic!("response body must be readable: {error}"));
        let text = String::from_utf8(body.to_vec())
            .unwrap_or_else(|error| panic!("response body must be UTF-8 JSON: {error}"));
        assert!(!text.contains("upstream-secret-token"));
        assert_eq!(text, r#"{"detail":"Failed to fetch usage from upstream"}"#);
    }

    #[test]
    fn cors_allows_only_local_origins_and_explicit_api_headers() {
        assert!(is_local_origin(&HeaderValue::from_static(
            "http://localhost:5173"
        )));
        assert!(is_local_origin(&HeaderValue::from_static(
            "https://127.0.0.1:8443"
        )));
        assert!(!is_local_origin(&HeaderValue::from_static(
            "https://attacker.example"
        )));
        assert!(!is_local_origin(&HeaderValue::from_static(
            "http://localhost.attacker.example"
        )));

        let router = Router::new()
            .route("/", get(|| async { "ok" }))
            .layer(local_cors());
        let request = Request::builder()
            .method(Method::OPTIONS)
            .uri("/")
            .header(header::ORIGIN, "http://localhost:3000")
            .header(header::ACCESS_CONTROL_REQUEST_METHOD, "POST")
            .header(
                header::ACCESS_CONTROL_REQUEST_HEADERS,
                "authorization, x-api-key",
            )
            .body(Body::empty())
            .unwrap_or_else(|error| panic!("preflight request must build: {error}"));
        let response = futures::executor::block_on(router.oneshot(request))
            .unwrap_or_else(|never| match never {});
        assert_eq!(
            response.headers()[header::ACCESS_CONTROL_ALLOW_ORIGIN],
            "http://localhost:3000"
        );
        assert!(
            response.headers()[header::ACCESS_CONTROL_ALLOW_HEADERS]
                .to_str()
                .is_ok_and(
                    |headers| headers.contains("authorization") && headers.contains("x-api-key")
                )
        );
    }

    #[test]
    fn empty_account_is_503_by_design() {
        let response = (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"detail":"No initialized account available"})),
        )
            .into_response();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    use crate::test_log::CapturedLogs;

    fn request(method: Method, uri: &str, body: Body) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(uri)
            .header(header::CONTENT_TYPE, "application/json")
            .body(body)
            .unwrap_or_else(|error| panic!("request build failed: {error}"))
    }

    async fn body_bytes(response: Response) -> bytes::Bytes {
        to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap_or_else(|error| panic!("body must be readable: {error}"))
    }

    async fn run_logged(router: Router, request: Request<Body>) -> (Response, CapturedLogs) {
        let (logs, _guard) = CapturedLogs::install();
        let response = router
            .layer(middleware::from_fn(failed_response_log_middleware))
            .oneshot(request)
            .await
            .unwrap_or_else(|never| match never {});
        (response, logs)
    }

    #[tokio::test]
    async fn failed_responses_are_logged_with_detail_that_never_reaches_the_client() {
        let router = Router::new().route(
            "/boom",
            get(|| async {
                ErrorDetail::attach(
                    (StatusCode::BAD_GATEWAY, "public message").into_response(),
                    "upstream error 502 [UNKNOWN]: secret detail",
                )
            }),
        );
        let (mut response, logs) =
            run_logged(router, request(Method::GET, "/boom", Body::empty())).await;

        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        assert!(ErrorDetail::take(&mut response).is_none());
        assert_eq!(&body_bytes(response).await[..], b"public message");
        assert!(
            logs.has("ERROR", &["GET /boom failed", "secret detail"]),
            "5xx must be logged at ERROR with its detail: {:?}",
            logs.lines()
        );
    }

    #[tokio::test]
    async fn rejections_without_detail_log_their_small_body_and_keep_it_intact() {
        let router = Router::new().route(
            "/reject",
            get(|| async { (StatusCode::UNAUTHORIZED, "Invalid or missing API Key") }),
        );
        let (response, logs) =
            run_logged(router, request(Method::GET, "/reject", Body::empty())).await;

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            &body_bytes(response).await[..],
            b"Invalid or missing API Key"
        );
        assert!(
            logs.has(
                "WARN",
                &["GET /reject rejected", "Invalid or missing API Key"]
            ),
            "4xx must be logged at WARN with its body text: {:?}",
            logs.lines()
        );
    }

    #[tokio::test]
    async fn large_and_streaming_error_bodies_pass_through_unbuffered() {
        let large = "x".repeat(8192);
        let large_for_route = large.clone();
        let router = Router::new()
            .route(
                "/large",
                get(move || {
                    let body = large_for_route.clone();
                    async move { (StatusCode::BAD_REQUEST, body) }
                }),
            )
            .route(
                "/stream",
                get(|| async {
                    let chunks = futures::stream::iter([
                        Ok::<_, std::convert::Infallible>(bytes::Bytes::from_static(b"part1 ")),
                        Ok(bytes::Bytes::from_static(b"part2")),
                    ]);
                    (StatusCode::BAD_GATEWAY, Body::from_stream(chunks))
                }),
            );

        let (response, logs) = run_logged(
            router.clone(),
            request(Method::GET, "/large", Body::empty()),
        )
        .await;
        assert_eq!(body_bytes(response).await, large.as_bytes());
        assert!(
            logs.has("WARN", &["GET /large rejected", "Bad Request"]),
            "oversized bodies fall back to the status reason: {:?}",
            logs.lines()
        );

        let (response, logs) =
            run_logged(router, request(Method::GET, "/stream", Body::empty())).await;
        assert_eq!(&body_bytes(response).await[..], b"part1 part2");
        assert!(
            logs.has("ERROR", &["GET /stream failed", "Bad Gateway"]),
            "streaming bodies are never buffered: {:?}",
            logs.lines()
        );
    }

    #[tokio::test]
    async fn successful_responses_are_not_logged_and_404_is_debug_only() {
        let router = Router::new().route("/ok", get(|| async { "ok" }));
        let (response, logs) =
            run_logged(router.clone(), request(Method::GET, "/ok", Body::empty())).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            logs.lines().is_empty(),
            "2xx must not be logged: {:?}",
            logs.lines()
        );

        let (response, logs) =
            run_logged(router, request(Method::GET, "/favicon.ico", Body::empty())).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert!(
            logs.has("DEBUG", &["GET /favicon.ico rejected"]),
            "{:?}",
            logs.lines()
        );
        assert!(
            !logs.lines().iter().any(|line| line.starts_with("WARN")),
            "404 must not be a warning: {:?}",
            logs.lines()
        );
    }

    // Full router with state built directly (no background model refresh).
    fn test_app() -> Router {
        let config = Arc::new(Config {
            proxy_api_key: "test-key".into(),
            ..Config::default()
        });
        let auth_manager = Arc::new(
            AuthManager::new((*config).clone())
                .unwrap_or_else(|error| panic!("auth manager must construct: {error}")),
        );
        let http_client = Arc::new(
            KiroHttpClient::new(auth_manager.clone(), &config)
                .unwrap_or_else(|error| panic!("http client must construct: {error}")),
        );
        let model_cache = Arc::new(ModelInfoCache::default());
        model_cache.load_fallback();
        let model_resolver = Arc::new(ModelResolver::from_config((*model_cache).clone(), &config));
        app(AppState {
            compat: Arc::new(CompatibilityPipeline::new()),
            config,
            auth_manager,
            http_client,
            model_cache,
            model_resolver,
            truncation_store: Arc::new(TruncationStore::default()),
        })
    }

    #[tokio::test]
    async fn real_routes_log_rejections_end_to_end() {
        let (logs, _guard) = CapturedLogs::install();
        let app = test_app();

        let unauthorized = app
            .clone()
            .oneshot(request(
                Method::POST,
                "/v1/chat/completions",
                Body::from(r#"{"model":"m","messages":[{"role":"user","content":"hi"}]}"#),
            ))
            .await
            .unwrap_or_else(|never| match never {});
        assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);

        let mut empty = request(
            Method::POST,
            "/v1/chat/completions",
            Body::from(r#"{"model":"m","messages":[]}"#),
        );
        empty.headers_mut().insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer test-key"),
        );
        let mut empty = app
            .oneshot(empty)
            .await
            .unwrap_or_else(|never| match never {});
        assert_eq!(empty.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert!(ErrorDetail::take(&mut empty).is_none());

        assert!(
            logs.has(
                "WARN",
                &["POST /v1/chat/completions rejected", "status=401"]
            ),
            "{:?}",
            logs.lines()
        );
        assert!(
            logs.has(
                "WARN",
                &[
                    "POST /v1/chat/completions rejected",
                    "status=422",
                    "messages must contain at least one message"
                ]
            ),
            "{:?}",
            logs.lines()
        );
        assert_eq!(
            logs.lines()
                .iter()
                .filter(|line| line.contains("rejected") || line.contains("failed"))
                .count(),
            2,
            "each failure is logged exactly once: {:?}",
            logs.lines()
        );
    }
}
