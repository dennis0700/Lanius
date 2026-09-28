//! axum route handlers for the OpenAI Chat Completions API.
//!
//! Wires `/`, `/health`, `/v1/models`, and `/v1/chat/completions` to the
//! shared [`crate::auth::AuthManager`] and Kiro upstream client.
//! Responsibilities specific to this module:
//! - Bearer token authentication (`Authorization: Bearer <key>`).
//! - Resolving client-facing model ids/aliases via
//!   [`crate::model::resolver::ModelResolver`] before dispatching to Kiro.
//! - Converting the request into the Kiro payload via
//!   [`build_kiro_payload`] and issuing the upstream request.
//! - Injecting a synthetic `web_search` tool when enabled, and rewriting
//!   requests to inject truncation-recovery notices (see
//!   [`crate::truncation`]).
//! - Producing either a buffered JSON response or a streamed SSE response
//!   (delegating SSE framing to [`super::sse`]).

use axum::{
    Json, Router,
    body::Body,
    extract::{State, rejection::JsonRejection},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use std::future::Future;
use std::sync::Arc;

use futures_util::{StreamExt, stream};
use serde_json::{Value, json};

use crate::api::anthropic::sse::DEFAULT_PING_INTERVAL;
use crate::api::openai::models::{
    ChatCompletionRequest, ChatMessage, ModelList, OpenAIMessageContent, OpenAIModel, Tool,
    ToolFunction,
};
use crate::api::openai::sse::{OpenAiFormatContext, collect_openai_response, encode_openai_sse};
use crate::auth::AuthManager;
use crate::compat::ToolNameAliases;
use crate::config::{APP_TITLE, APP_VERSION, Config};
use crate::convert::build_kiro_payload;
use crate::error::GatewayError;
use crate::model::{ModelInfoCache, ModelResolver};
use crate::truncation::{
    TruncationStore, generate_truncation_user_message, prepend_tool_recovery_notice,
};
use crate::upstream::{KiroHttpClient, parse_kiro_stream};
use crate::utils::{HashableMessage, generate_conversation_id};

/// Shared axum state for the OpenAI routes: gateway config, the shared
/// [`AuthManager`] and model catalog cache, the [`ModelResolver`] used to
/// map client-facing model ids to internal Kiro model ids (and vice versa
/// for listing), and the truncation-recovery store shared across
/// requests.
#[derive(Clone)]
pub struct OpenAiState {
    /// Effective gateway configuration.
    pub config: Config,
    /// Handles token acquisition/refresh against Kiro/AWS.
    pub auth_manager: Arc<AuthManager>,
    /// Cached model catalog (ids, context limits, reasoning capabilities).
    pub model_cache: Arc<ModelInfoCache>,
    /// Resolves client-supplied model names/aliases to Kiro model ids.
    pub model_resolver: ModelResolver,
    /// Recovery records for upstream tool-call/content truncation.
    pub truncation_store: TruncationStore,
}

impl OpenAiState {
    /// Builds a fresh [`OpenAiState`] from gateway config and a shared
    /// [`AuthManager`], constructing a new (empty) model cache and
    /// deriving the [`ModelResolver`] from it.
    ///
    /// # Examples
    ///
    /// ```
    /// use lanius_core::api::OpenAiState;
    /// use lanius_core::auth::AuthManager;
    /// use lanius_core::Config;
    /// use std::sync::Arc;
    ///
    /// let config = Config::default();
    /// let auth = Arc::new(AuthManager::new(config.clone()).expect("valid config"));
    /// let _state = OpenAiState::new(config, auth);
    /// ```
    pub fn new(config: Config, auth_manager: Arc<AuthManager>) -> Self {
        let cache = Arc::new(ModelInfoCache::default());
        Self {
            model_resolver: ModelResolver::from_config((*cache).clone(), &config),
            model_cache: cache,
            auth_manager,
            truncation_store: TruncationStore::default(),
            config,
        }
    }
}

/// Builds the axum [`Router`] exposing the OpenAI-compatible endpoints
/// plus lightweight `/` and `/health` probes.
///
/// # Examples
///
/// ```
/// use lanius_core::api::openai_router;
///
/// let _router = openai_router();
/// ```
pub fn router() -> Router<OpenAiState> {
    Router::new()
        .route("/", get(root))
        .route("/health", get(health))
        .route("/v1/models", get(models))
        .route("/v1/chat/completions", post(chat_completions))
}

/// `GET /` — a minimal unauthenticated liveness/info probe.
async fn root() -> Json<Value> {
    Json(json!({
        "status": "ok",
        "message": format!("{APP_TITLE} is running"),
        "version": APP_VERSION,
    }))
}

/// `GET /health` — a minimal unauthenticated health-check probe.
async fn health() -> Json<Value> {
    Json(json!({
        "status": "healthy",
        "timestamp": chrono::Utc::now().to_rfc3339(),
        "version": APP_VERSION,
    }))
}

/// `GET /v1/models` — lists models currently available to this gateway
/// (as resolved by [`ModelResolver`]), requires a valid bearer token.
async fn models(State(state): State<OpenAiState>, headers: HeaderMap) -> Response {
    if let Err(response) = authorize(&headers, &state.config.proxy_api_key) {
        return *response;
    }
    let data = state
        .model_resolver
        .get_available_model_details()
        .into_iter()
        .map(|details| OpenAIModel {
            id: details.id,
            object: "model".to_string(),
            created: chrono::Utc::now().timestamp(),
            owned_by: "anthropic".to_string(),
            description: format_model_description(
                details.description.as_deref(),
                details.rate_multiplier,
            ),
            supports_thinking: details.supports_thinking,
        })
        .collect();
    Json(ModelList {
        object: "list".to_string(),
        data,
    })
    .into_response()
}

/// Formats a human-readable model description, prefixing the credit-rate
/// multiplier (e.g. `"1.30x credits"`) when known and falling back to a
/// generic description if neither a rate nor a description is available.
fn format_model_description(
    description: Option<&str>,
    rate_multiplier: Option<f64>,
) -> Option<String> {
    let rate_text = rate_multiplier.map(|rate| format!("{rate:.2}x credits"));
    match (rate_text, description) {
        (Some(rate), Some(description)) => Some(format!("{rate} \u{2013} {description}")),
        (Some(rate), None) => Some(rate),
        (None, Some(description)) => Some(description.to_string()),
        (None, None) => Some("Claude model via Kiro API".to_string()),
    }
}

/// Handles `POST /v1/chat/completions`.
///
/// Validates the bearer token and non-empty message list, applies
/// truncation recovery rewrites and (optionally) injects a synthetic
/// `web_search` tool, resolves the client-facing model id to Kiro's
/// internal id, then prepares the upstream connection. Then either
/// streams an SSE response or collects and returns a single JSON
/// response depending on `request.stream`. Token/format state
/// ([`OpenAiFormatContext`]) is built from the *original* (pre-resolution)
/// model name and messages so token accounting and echoed model names
/// match what the client sent.
async fn chat_completions(
    State(state): State<OpenAiState>,
    headers: HeaderMap,
    request: std::result::Result<Json<ChatCompletionRequest>, JsonRejection>,
) -> Response {
    if let Err(response) = authorize(&headers, &state.config.proxy_api_key) {
        return *response;
    }
    let mut request = match request {
        Ok(Json(request)) if !request.messages.is_empty() => request,
        Ok(_) => return validation_error("messages must contain at least one message"),
        Err(rejection) => return validation_error(&rejection.to_string()),
    };

    let original_model = request.model.clone();
    let original_messages = messages_to_values(&request.messages);
    let conversation_id = conversation_id_for(&request.messages);
    inject_truncation_recovery(&mut request, &state.truncation_store, &conversation_id);
    inject_web_search_tool(&mut request, state.config.web_search_enabled);

    request.model = state.model_resolver.resolve(&request.model).internal_id;
    let prepared = match prepare_openai_request(&state, &request, &conversation_id).await {
        Ok(prepared) => prepared,
        Err(error) => return gateway_error_response(error),
    };

    let format_context = OpenAiFormatContext::new(
        original_model,
        (*state.model_cache).clone(),
        original_messages,
        tools_to_values(request.tools.as_deref()),
        state.config.clone(),
        conversation_id,
        state.truncation_store.clone(),
    )
    .with_tool_name_aliases(prepared.tool_name_aliases);
    match prepared.upstream {
        OpenAiUpstream::Stream(source) => streaming_response(source, format_context),
        OpenAiUpstream::Response(response) => {
            match collect_openai_response(response.bytes_stream(), format_context).await {
                Ok(response) => Json(response).into_response(),
                Err(error) => gateway_error_response(error),
            }
        }
    }
}

/// The outcome of [`prepare_openai_request`]: either a buffered upstream
/// response or an open byte stream, plus the per-request tool name alias
/// table needed to finish formatting the response.
struct PreparedOpenAiRequest {
    upstream: OpenAiUpstream,
    tool_name_aliases: ToolNameAliases,
}

/// The upstream connection prepared for a request: a single buffered
/// response for non-streaming requests, or an already-preflighted byte
/// stream for streaming requests.
enum OpenAiUpstream {
    Response(reqwest::Response),
    Stream(
        futures_util::stream::BoxStream<'static, std::result::Result<bytes::Bytes, reqwest::Error>>,
    ),
}

/// Converts the request to the Kiro payload and opens an upstream
/// connection (buffered for non-streaming requests, preflighted for
/// streaming requests).
///
/// Refreshes the access token, resolves the Kiro profile ARN, builds the
/// Kiro-shaped payload (allocating fresh per-request tool name aliases),
/// and issues the request (streamed preflight or buffered, depending on
/// `request.stream`).
async fn prepare_openai_request(
    state: &OpenAiState,
    request: &ChatCompletionRequest,
    conversation_id: &str,
) -> crate::error::Result<PreparedOpenAiRequest> {
    state.auth_manager.access_token_and_autofetch().await?;
    let profile_arn = state
        .auth_manager
        .profile_arn()
        .await
        .or_else(|| state.config.profile_arn.clone());
    let mut tool_name_aliases = ToolNameAliases::default();
    let payload = build_kiro_payload(
        request,
        conversation_id,
        profile_arn.as_deref(),
        &state.config,
        &state.model_cache,
        &mut tool_name_aliases,
    )?
    .payload;
    let url = format!(
        "{}/generateAssistantResponse",
        state.auth_manager.api_host().await
    );
    let upstream = if request.stream {
        preflight_openai_stream(state.auth_manager.clone(), &state.config, url, payload)
            .await
            .map(OpenAiUpstream::Stream)?
    } else {
        let client = KiroHttpClient::new(state.auth_manager.clone(), &state.config)?;
        client
            .request_with_retry(reqwest::Method::POST, &url, Some(payload), None, true)
            .await
            .map(OpenAiUpstream::Response)?
    };
    Ok(PreparedOpenAiRequest {
        upstream,
        tool_name_aliases,
    })
}

/// Issues the upstream `generateAssistantResponse` request for a streaming
/// chat completion and waits for the first byte (via
/// [`preflight_first_byte`]) before returning the stream, so connection-
/// level failures surface before any bytes are sent to the client.
async fn preflight_openai_stream(
    auth_manager: std::sync::Arc<crate::auth::AuthManager>,
    config: &Config,
    url: String,
    payload: Value,
) -> crate::error::Result<
    futures_util::stream::BoxStream<'static, std::result::Result<bytes::Bytes, reqwest::Error>>,
> {
    preflight_first_byte(
        config.first_token_max_retries,
        config.first_token_timeout,
        move || {
            let auth_manager = auth_manager.clone();
            let config = config.clone();
            let url = url.clone();
            let payload = payload.clone();
            async move {
                let client = KiroHttpClient::new(auth_manager, &config)?;
                let response = client
                    .request_with_retry(reqwest::Method::POST, &url, Some(payload), None, true)
                    .await?;
                Ok(response.bytes_stream().boxed())
            }
        },
    )
    .await
}

/// Generic first-byte preflight helper: calls `request` (a factory closure
/// so a fresh HTTP request can be issued on each retry) up to `retries`
/// times, waiting up to `timeout` for the first byte of each attempt. On
/// success, the first byte is buffered and re-prepended to the returned
/// stream (via `stream::once(...).chain(...)`) so no data is lost. Times
/// out with [`GatewayError::FirstTokenTimeout`] if no attempt produces a
/// first byte in time.
async fn preflight_first_byte<F, Fut>(
    retries: u32,
    timeout: std::time::Duration,
    mut request: F,
) -> crate::error::Result<
    futures_util::stream::BoxStream<'static, std::result::Result<bytes::Bytes, reqwest::Error>>,
>
where
    F: FnMut() -> Fut,
    Fut: Future<
        Output = crate::error::Result<
            futures_util::stream::BoxStream<
                'static,
                std::result::Result<bytes::Bytes, reqwest::Error>,
            >,
        >,
    >,
{
    let tries = retries.max(1);
    for attempt in 0..tries {
        let mut source = request().await?;
        match tokio::time::timeout(timeout, source.next()).await {
            Ok(Some(Ok(first))) => {
                return Ok(
                    stream::once(async move { Ok::<bytes::Bytes, reqwest::Error>(first) })
                        .chain(source)
                        .boxed(),
                );
            }
            Ok(Some(Err(error))) => {
                // Nothing has been sent to the client yet, so a transient
                // failure on the first chunk can be retried with a fresh request.
                match GatewayError::from(error) {
                    GatewayError::Network(info) if info.is_retryable && attempt + 1 < tries => {
                        tracing::warn!(
                            attempt = attempt + 1,
                            category = %info.category,
                            details = %info.technical_details,
                            "first chunk read failed; retrying upstream request"
                        );
                        continue;
                    }
                    error => return Err(error),
                }
            }
            Ok(None) => return Ok(source),
            Err(_) if attempt + 1 < tries => continue,
            Err(_) => return Err(GatewayError::FirstTokenTimeout(timeout)),
        }
    }
    Err(GatewayError::Internal(
        "first-token preflight exited unexpectedly".to_string(),
    ))
}

/// Builds the streaming `text/event-stream` [`Response`] for a request,
/// wiring the raw upstream byte stream through Kiro event parsing
/// ([`parse_kiro_stream`]) and OpenAI SSE encoding ([`encode_openai_sse`]).
/// If the encoder itself errors mid-stream, emits a `data: [DONE]` frame
/// and terminates rather than propagating the error into the HTTP body.
fn streaming_response(
    source: futures_util::stream::BoxStream<
        'static,
        std::result::Result<bytes::Bytes, reqwest::Error>,
    >,
    context: OpenAiFormatContext,
) -> Response {
    let events = parse_kiro_stream(
        source,
        context.config.first_token_timeout,
        context.config.streaming_read_timeout,
    );
    let frames = encode_openai_sse(events, context);
    let body_stream = async_stream::stream! {
        futures_util::pin_mut!(frames);
        // SSE comment heartbeats keep idle connections alive while the
        // upstream is still generating (e.g. tool calls, which are only
        // emitted once complete). Clients ignore comment lines.
        let mut pings = tokio::time::interval(DEFAULT_PING_INTERVAL);
        pings.tick().await;
        loop {
            tokio::select! {
                _ = pings.tick() => {
                    yield Ok::<_, std::convert::Infallible>(bytes::Bytes::from_static(b": keepalive\n\n"));
                }
                frame = frames.next() => match frame {
                    Some(Ok(frame)) => yield Ok(bytes::Bytes::from(frame)),
                    Some(Err(error)) => {
                        tracing::warn!(error = %error, "OpenAI SSE encoder failed");
                        yield Ok(bytes::Bytes::from_static(b"data: [DONE]\n\n"));
                        break;
                    }
                    None => break,
                }
            }
        }
    };
    let mut response = Body::from_stream(body_stream).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    response
}

/// Validates the request's `Authorization: Bearer <key>` header against
/// the configured `proxy_api_key`, comparing in constant time via
/// [`constant_time_eq`]. Returns `Err` with a boxed 401 response on
/// failure (boxed to keep the `Ok` case's size small, since `Response` is
/// large).
fn authorize(headers: &HeaderMap, proxy_api_key: &str) -> std::result::Result<(), Box<Response>> {
    let supplied = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    let expected = format!("Bearer {proxy_api_key}");
    if constant_time_eq(supplied.as_bytes(), expected.as_bytes()) {
        Ok(())
    } else {
        Err(Box::new(
            (
                StatusCode::UNAUTHORIZED,
                Json(json!({"detail": "Invalid or missing API Key"})),
            )
                .into_response(),
        ))
    }
}

/// Compares two byte slices for equality in constant time (with respect
/// to the *longer* slice's length) to mitigate timing side-channels
/// during API key comparison.
fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    let mut difference = left.len() ^ right.len();
    let longest = left.len().max(right.len());
    for index in 0..longest {
        let a = left.get(index).copied().unwrap_or(0);
        let b = right.get(index).copied().unwrap_or(0);
        difference |= usize::from(a ^ b);
    }
    difference == 0
}

/// Maps a [`GatewayError`] to an OpenAI-ish JSON error response.
/// [`GatewayError::Upstream`] preserves the original upstream HTTP status
/// and reports it as a `kiro_api_error`; other variants fall back to
/// simpler `{"detail": ...}` bodies with a status derived from
/// `error.http_status()`.
fn gateway_error_response(error: GatewayError) -> Response {
    let status =
        StatusCode::from_u16(error.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    match error {
        GatewayError::Upstream {
            status: upstream_status,
            info,
        } => {
            let status = StatusCode::from_u16(upstream_status).unwrap_or(StatusCode::BAD_GATEWAY);
            (status, Json(json!({
                "error": {"message": info.user_message, "type": "kiro_api_error", "code": upstream_status}
            }))).into_response()
        }
        GatewayError::InvalidRequest(message) => {
            (StatusCode::BAD_REQUEST, Json(json!({"detail": message}))).into_response()
        }
        _ => (status, Json(json!({"detail": "Internal Server Error"}))).into_response(),
    }
}

/// Builds the 422 response returned for malformed or empty-message
/// requests.
fn validation_error(message: &str) -> Response {
    (
        StatusCode::UNPROCESSABLE_ENTITY,
        Json(json!({"detail": message, "body": ""})),
    )
        .into_response()
}

/// Converts chat messages to plain [`Value`]s (silently dropping any
/// message that fails to serialize), for use in token estimation and
/// conversation id hashing.
fn messages_to_values(messages: &[ChatMessage]) -> Vec<Value> {
    messages
        .iter()
        .filter_map(|message| serde_json::to_value(message).ok())
        .collect()
}

/// Converts request tools to plain [`Value`]s for use in token estimation.
fn tools_to_values(tools: Option<&[Tool]>) -> Option<Vec<Value>> {
    tools.map(|tools| {
        tools
            .iter()
            .filter_map(|tool| serde_json::to_value(tool).ok())
            .collect()
    })
}

/// Derives a stable conversation identifier from the request's message
/// history, used as the [`TruncationStore`] key so truncation recovery
/// state can be matched across turns of the same conversation.
fn conversation_id_for(messages: &[ChatMessage]) -> String {
    let values = messages_to_values(messages);
    let hashable: Vec<HashableMessage<'_>> = messages
        .iter()
        .zip(values.iter())
        .map(|(message, content)| HashableMessage {
            role: &message.role,
            content,
        })
        .collect();
    generate_conversation_id(&hashable)
}

/// Rewrites the request's messages in place to recover from previously
/// observed output truncation, using truncation records saved by
/// [`super::sse`] during earlier turns of this conversation (keyed by
/// `conversation_id`).
///
/// Two recovery paths, applied per message:
/// - A `tool` role message whose `tool_call_id` matches a saved tool
///   truncation gets its `content` replaced with the original text plus a
///   prepended recovery notice, so the model is informed that the
///   previous tool output it saw was cut short.
/// - An `assistant` message whose plain-text content matches a saved
///   content truncation gets a synthetic follow-up `user` message
///   appended asking the model to continue, since the assistant's own
///   turn was cut off mid-generation.
///
/// Unconditionally looks up the store (unlike the Anthropic equivalent,
/// which checks `config.truncation_recovery` up front) — if truncation
/// recovery was disabled, [`super::sse::save_truncations`] never wrote any
/// records, so these lookups simply find nothing and this becomes a no-op.
fn inject_truncation_recovery(
    request: &mut ChatCompletionRequest,
    store: &TruncationStore,
    conversation_id: &str,
) {
    let mut recovered = Vec::with_capacity(request.messages.len());
    for message in &request.messages {
        if message.role == "tool" {
            if let Some(id) = message.tool_call_id.as_deref() {
                if store.get_tool_truncation(conversation_id, id).is_some() {
                    let mut replacement = message.clone();
                    let original = message_content_text(message.content.as_ref());
                    replacement.content = Some(OpenAIMessageContent::Text(
                        prepend_tool_recovery_notice(&original),
                    ));
                    recovered.push(replacement);
                    continue;
                }
            }
        }
        recovered.push(message.clone());
        if message.role == "assistant" {
            if let Some(OpenAIMessageContent::Text(content)) = message.content.as_ref() {
                if store
                    .get_content_truncation(conversation_id, content)
                    .is_some()
                {
                    recovered.push(ChatMessage {
                        role: "user".to_string(),
                        content: Some(OpenAIMessageContent::Text(
                            generate_truncation_user_message().to_string(),
                        )),
                        name: None,
                        tool_calls: None,
                        tool_call_id: None,
                    });
                }
            }
        }
    }
    request.messages = recovered;
}

/// Extracts a message's plain-text content for truncation matching,
/// stringifying non-text content (blocks/other) rather than concatenating
/// text fragments as the Anthropic equivalent does — since OpenAI
/// truncation notices are matched against the whole `content` value.
fn message_content_text(content: Option<&OpenAIMessageContent>) -> String {
    match content {
        Some(OpenAIMessageContent::Text(content)) => content.clone(),
        Some(OpenAIMessageContent::Blocks(content)) => Value::Array(content.clone()).to_string(),
        Some(OpenAIMessageContent::Other(content)) => content.to_string(),
        None => String::new(),
    }
}

/// Appends a synthetic `web_search` function tool to the request when
/// `enabled` is `true`, unless the client already declared a tool with
/// that exact name — avoiding a duplicate tool definition being sent
/// upstream.
fn inject_web_search_tool(request: &mut ChatCompletionRequest, enabled: bool) {
    if !enabled {
        return;
    }
    let tools = request.tools.get_or_insert_with(Vec::new);
    let already_present = tools.iter().any(|tool| {
        tool.kind == "function"
            && tool
                .function
                .as_ref()
                .is_some_and(|function| function.name == "web_search")
    });
    if !already_present {
        tools.push(Tool {
            kind: "function".to_string(),
            function: Some(ToolFunction {
                name: "web_search".to_string(),
                description: Some("Search the web for current information. Use when you need up-to-date data from the internet.".to_string()),
                parameters: Some(serde_json::from_value(json!({
                    "type": "object",
                    "properties": {"query": {"type": "string", "description": "Search query"}},
                    "required": ["query"],
                })).unwrap_or_default()),
            }),
            name: None,
            description: None,
            input_schema: None,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authorization_is_strict_and_constant_work_accepts_only_exact_bearer_header() {
        let key = "correct key";
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer correct key"),
        );
        assert!(authorize(&headers, key).is_ok());
        for candidate in [
            "Bearer  correct key",
            "bearer correct key",
            "correct key",
            "Bearer wrong",
        ] {
            let mut headers = HeaderMap::new();
            let value = HeaderValue::from_str(candidate).expect("test header is valid");
            headers.insert(header::AUTHORIZATION, value);
            assert!(authorize(&headers, key).is_err(), "{candidate}");
        }
        assert!(!constant_time_eq(b"prefix", b"prefix-extended"));
    }

    #[test]
    fn public_routes_and_model_resolver_list_are_configured() {
        let _router: Router<OpenAiState> = router();
        let config = Config {
            hidden_from_list: vec!["hidden".to_string()],
            model_aliases: std::collections::HashMap::from([(
                "alias".to_string(),
                "visible".to_string(),
            )]),
            ..Config::default()
        };
        let cache = ModelInfoCache::default();
        cache.update(vec![
            json!({"modelId": "hidden"}),
            json!({"modelId": "visible"}),
        ]);
        let state = OpenAiState {
            auth_manager: Arc::new(
                AuthManager::new(config.clone()).expect("auth manager must construct"),
            ),
            model_cache: Arc::new(cache.clone()),
            model_resolver: ModelResolver::from_config(cache, &config),
            truncation_store: TruncationStore::default(),
            config,
        };
        let ids = state.model_resolver.get_available_models();
        assert!(!ids.contains(&"hidden".to_string()));
        assert!(ids.contains(&"alias".to_string()));
    }

    #[test]
    fn web_search_injection_and_truncation_notice_are_safe() {
        let mut request: ChatCompletionRequest = serde_json::from_value(json!({
            "model": "m", "messages": [{"role": "tool", "tool_call_id": "call", "content": "result"}]
        })).expect("fixture is valid request");
        inject_web_search_tool(&mut request, true);
        assert_eq!(request.tools.as_ref().map(Vec::len), Some(1));
        let store = TruncationStore::default();
        let conversation = conversation_id_for(&request.messages);
        store.save_tool_truncation(&conversation, "call", "tool", json!({"reason": "cut"}));
        inject_truncation_recovery(&mut request, &store, &conversation);
        assert!(
            message_content_text(request.messages[0].content.as_ref()).contains("[API Limitation]")
        );
    }
    #[test]
    fn model_description_combines_rate_and_text_or_falls_back() {
        assert_eq!(
            format_model_description(
                Some("Claude Sonnet 4 model with 1M context window"),
                Some(1.3)
            ),
            Some("1.30x credits \u{2013} Claude Sonnet 4 model with 1M context window".to_string())
        );
        assert_eq!(
            format_model_description(None, Some(2.2)),
            Some("2.20x credits".to_string())
        );
        assert_eq!(
            format_model_description(Some("Bespoke description"), None),
            Some("Bespoke description".to_string())
        );
        assert_eq!(
            format_model_description(None, None),
            Some("Claude model via Kiro API".to_string())
        );
    }
}

#[cfg(test)]
mod failover_and_preflight_regression_tests {
    use super::*;
    use std::collections::VecDeque;
    use std::time::Duration;

    #[tokio::test]
    async fn first_byte_timeout_retries_before_returning_a_stream() {
        let mut sources = VecDeque::from([
            stream::pending::<std::result::Result<bytes::Bytes, reqwest::Error>>().boxed(),
            stream::once(async {
                Ok::<bytes::Bytes, reqwest::Error>(bytes::Bytes::from_static(b"first"))
            })
            .boxed(),
        ]);
        let mut requests = 0_u8;
        let mut source =
            preflight_first_byte(2, Duration::from_millis(1), || {
                requests = requests.saturating_add(1);
                let next = sources.pop_front();
                async move {
                    next.ok_or_else(|| GatewayError::Internal("missing test stream".to_string()))
                }
            })
            .await
            .unwrap_or_else(|error| panic!("preflight should retry before emitting: {error}"));
        assert_eq!(requests, 2);
        let first = source
            .next()
            .await
            .unwrap_or_else(|| panic!("replayed first byte must be available"))
            .unwrap_or_else(|error| panic!("test stream must be successful: {error}"));
        assert_eq!(first, bytes::Bytes::from_static(b"first"));
    }

    // Serves one response that sends headers and then drops the connection
    // before any body chunk, so reading the first chunk yields a real
    // `reqwest::Error`.
    async fn truncated_body_stream()
    -> futures_util::stream::BoxStream<'static, std::result::Result<bytes::Bytes, reqwest::Error>>
    {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap_or_else(|error| panic!("bind test listener: {error}"));
        let addr = listener
            .local_addr()
            .unwrap_or_else(|error| panic!("listener address: {error}"));
        tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = [0_u8; 1024];
                let _ = socket.read(&mut buf).await;
                let _ = socket
                    .write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n")
                    .await;
            }
        });
        reqwest::get(format!("http://{addr}/"))
            .await
            .unwrap_or_else(|error| panic!("test request: {error}"))
            .bytes_stream()
            .boxed()
    }

    #[tokio::test]
    async fn first_chunk_network_error_retries_before_returning_a_stream() {
        let mut sources = VecDeque::from([
            truncated_body_stream().await,
            stream::once(async {
                Ok::<bytes::Bytes, reqwest::Error>(bytes::Bytes::from_static(b"first"))
            })
            .boxed(),
        ]);
        let mut requests = 0_u8;
        let mut source =
            preflight_first_byte(2, Duration::from_secs(5), || {
                requests = requests.saturating_add(1);
                let next = sources.pop_front();
                async move {
                    next.ok_or_else(|| GatewayError::Internal("missing test stream".to_string()))
                }
            })
            .await
            .unwrap_or_else(|error| {
                panic!("preflight should retry a dropped first chunk: {error}")
            });
        assert_eq!(requests, 2);
        let first = source
            .next()
            .await
            .unwrap_or_else(|| panic!("replayed first byte must be available"))
            .unwrap_or_else(|error| panic!("test stream must be successful: {error}"));
        assert_eq!(first, bytes::Bytes::from_static(b"first"));
    }

    #[tokio::test]
    async fn first_chunk_network_error_surfaces_after_last_attempt() {
        let mut requests = 0_u8;
        let result = preflight_first_byte(2, Duration::from_secs(5), || {
            requests = requests.saturating_add(1);
            async { Ok(truncated_body_stream().await) }
        })
        .await;
        assert_eq!(requests, 2);
        assert!(matches!(result, Err(GatewayError::Network(_))));
    }
}
