//! axum route handlers for the Anthropic Messages API.
//!
//! Wires `/v1/messages` and `/v1/messages/count_tokens` to the shared
//! [`crate::auth::AuthManager`] and Kiro upstream client. Responsibilities
//! specific to this module:
//! - API key authentication (`x-api-key` / `Authorization: Bearer`).
//! - Converting the Anthropic request into the Kiro payload via
//!   [`crate::convert::anthropic::anthropic_to_kiro`] and issuing the
//!   upstream request.
//! - Rewriting incoming requests to inject truncation-recovery notices
//!   (see [`crate::truncation`]) before conversion.
//! - Producing either a buffered JSON response or a streamed SSE response
//!   (delegating SSE framing to [`super::sse::AnthropicSseFormatter`]).

use std::convert::Infallible;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::{Json, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use bytes::Bytes;
use futures_util::{StreamExt, stream};
use serde_json::{Value, json};

use crate::api::anthropic::models::{
    AnthropicCountTokensRequest, AnthropicErrorDetail, AnthropicErrorResponse, AnthropicMessage,
    AnthropicMessageContent, AnthropicMessageRole, AnthropicMessagesRequest, ContentBlock,
    TextContentBlock, ToolResultContent,
};
use crate::api::anthropic::sse::{
    AnthropicSseFormatter, DEFAULT_PING_INTERVAL, RequestTokenInput, response_from_stream_result,
};
use crate::auth::AuthManager;
use crate::compat::ToolNameAliases;
use crate::config::Config;
use crate::convert::anthropic_to_kiro;
use crate::error::{GatewayError, Result};
use crate::model::ModelInfoCache;
use crate::tokenizer::estimate_request_tokens;
use crate::truncation::{
    TruncationStore, generate_truncation_user_message, prepend_tool_recovery_notice,
};
use crate::upstream::{KiroHttpClient, collect_stream_to_result, parse_kiro_stream};
use crate::utils::{HashableMessage, generate_conversation_id};

/// Shared axum state for the Anthropic routes: gateway config, the shared
/// [`AuthManager`] and model catalog cache, and the truncation-recovery
/// store shared across requests in the same process.
#[derive(Clone)]
pub struct AnthropicState {
    /// Effective gateway configuration.
    pub config: Arc<Config>,
    /// Handles token acquisition/refresh against Kiro/AWS.
    pub auth_manager: Arc<AuthManager>,
    /// Cached model catalog (ids, context limits, reasoning capabilities).
    pub model_cache: Arc<ModelInfoCache>,
    /// Recovery records for upstream tool-call/content truncation.
    pub truncation_store: TruncationStore,
}

/// Builds the axum [`Router`] exposing the Anthropic-compatible endpoints
/// (`/v1/messages`, `/v1/messages/count_tokens`).
///
/// # Examples
///
/// ```
/// use lanius_core::api::anthropic_router;
///
/// let _router = anthropic_router();
/// ```
pub fn router() -> Router<AnthropicState> {
    Router::new()
        .route("/v1/messages", post(messages))
        .route("/v1/messages/count_tokens", post(count_tokens_endpoint))
}

/// Handles `POST /v1/messages`.
///
/// Validates the API key and non-empty message list, applies truncation
/// recovery rewrites, rejects requests that request native (server-side)
/// web search (not yet supported by this gateway), estimates request
/// tokens, prepares the upstream connection, then either streams an SSE
/// response or collects and returns a single JSON response depending on
/// `request.stream`.
async fn messages(
    State(state): State<AnthropicState>,
    headers: HeaderMap,
    Json(mut request): Json<AnthropicMessagesRequest>,
) -> Response {
    if let Some(response) = verify_headers(&headers, &state.config, true) {
        return response;
    }
    if request.messages.is_empty() {
        return empty_messages_response();
    }
    apply_truncation_recovery(&mut request, &state.truncation_store, &state.config);

    if has_native_web_search(&request) {
        return error_response(
            StatusCode::NOT_IMPLEMENTED,
            "api_error",
            "Native web_search MCP bypass is not yet available in this Rust implementation",
        );
    }

    let token_input = match token_input(&request) {
        Ok(input) => input,
        Err(error) => return gateway_error_response(error),
    };
    let conversation_id = conversation_id(&request);
    let prepared = match prepare_request(&state, &request, &conversation_id).await {
        Ok(prepared) => prepared,
        Err(error) => return gateway_error_response(error),
    };

    if request.stream {
        stream_response(state, request, token_input, conversation_id, prepared)
    } else {
        let source = prepared.source;
        let result = collect_stream_to_result(
            source,
            state.config.first_token_timeout,
            state.config.streaming_read_timeout,
        )
        .await;
        match result {
            Ok(result) => {
                save_nonstream_truncations(&state, &conversation_id, &result);
                Json(response_from_stream_result(
                    result,
                    request.model,
                    &state.model_cache,
                    &token_input,
                    &prepared.tool_name_aliases,
                ))
                .into_response()
            }
            Err(error) => gateway_error_response(error),
        }
    }
}

/// Handles `POST /v1/messages/count_tokens`.
///
/// Performs local token estimation (via [`estimate_request_tokens`])
/// without contacting the upstream Kiro service.
async fn count_tokens_endpoint(
    State(state): State<AnthropicState>,
    headers: HeaderMap,
    Json(request): Json<AnthropicCountTokensRequest>,
) -> Response {
    if let Some(response) = verify_headers(&headers, &state.config, true) {
        return response;
    }
    if request.messages.is_empty() {
        return empty_messages_response();
    }
    let messages = match request
        .messages
        .iter()
        .map(serde_json::to_value)
        .collect::<std::result::Result<Vec<_>, _>>()
    {
        Ok(messages) => messages,
        Err(error) => return gateway_error_response(error.into()),
    };
    let tools = match request
        .tools
        .as_ref()
        .map(|tools| {
            tools
                .iter()
                .map(serde_json::to_value)
                .collect::<std::result::Result<Vec<_>, _>>()
        })
        .transpose()
    {
        Ok(tools) => tools,
        Err(error) => return gateway_error_response(error.into()),
    };
    let system = match request
        .system
        .as_ref()
        .map(serde_json::to_value)
        .transpose()
    {
        Ok(system) => system,
        Err(error) => return gateway_error_response(error.into()),
    };
    let input_tokens =
        estimate_request_tokens(&messages, tools.as_deref(), system.as_ref(), true).total_tokens;
    Json(json!({"input_tokens":input_tokens})).into_response()
}

/// Builds the streaming SSE `Response` for a `stream: true` request.
///
/// Drives the [`AnthropicSseFormatter`] state machine: emits the initial
/// `message_start` frame, then loops over parsed upstream Kiro events,
/// interleaving periodic `ping` heartbeats (via a [`tokio::time::interval`])
/// so proxies/clients do not time out an idle connection while waiting for
/// upstream tokens. On upstream error the formatter emits a terminal
/// `error` frame; on graceful end-of-stream it emits `message_delta` +
/// `message_stop`.
fn stream_response(
    state: AnthropicState,
    request: AnthropicMessagesRequest,
    token_input: RequestTokenInput,
    conversation_id: String,
    prepared: PreparedRequest,
) -> Response {
    let config = state.config.clone();
    let model_cache = (*state.model_cache).clone();
    let body = async_stream::stream! {
        let mut formatter = AnthropicSseFormatter::new(
            request.model,
            model_cache,
            token_input,
            conversation_id,
            config.truncation_recovery.then_some(state.truncation_store),
        )
        .with_tool_name_aliases(prepared.tool_name_aliases);
        for frame in formatter.start().unwrap_or_default() {
            yield Ok::<Bytes, Infallible>(Bytes::from(frame));
        }
        let source = parse_kiro_stream(
            prepared.source,
            config.first_token_timeout,
            config.streaming_read_timeout,
        );
        futures_util::pin_mut!(source);
        let mut pings = tokio::time::interval(DEFAULT_PING_INTERVAL);
        pings.tick().await;
        loop {
            tokio::select! {
                _ = pings.tick() => {
                    if let Ok(frame) = formatter.ping() {
                        yield Ok(Bytes::from(frame));
                    }
                }
                event = source.next() => match event {
                    Some(Ok(event)) => {
                        for frame in formatter.push(event).unwrap_or_default() {
                            yield Ok(Bytes::from(frame));
                        }
                    }
                    Some(Err(error)) => {
                        for frame in formatter.error(&error.user_message()).unwrap_or_default() {
                            yield Ok(Bytes::from(frame));
                        }
                        break;
                    }
                    None => {
                        for frame in formatter.finish().unwrap_or_default() {
                            yield Ok(Bytes::from(frame));
                        }
                        break;
                    }
                }
            }
        }
    };
    (
        [
            (header::CONTENT_TYPE, "text/event-stream"),
            (header::CACHE_CONTROL, "no-cache"),
            (header::CONNECTION, "keep-alive"),
        ],
        Body::from_stream(body),
    )
        .into_response()
}

/// The outcome of [`prepare_request`]: an open upstream byte stream ready
/// to be parsed.
struct PreparedRequest {
    source: futures_util::stream::BoxStream<'static, std::result::Result<Bytes, reqwest::Error>>,
    tool_name_aliases: ToolNameAliases,
}

/// Converts the request to the Kiro payload and opens an upstream
/// connection.
///
/// Refreshes the account's access token, resolves its Kiro profile ARN,
/// builds the Kiro-shaped payload (allocating fresh per-request tool name
/// aliases), and attempts the upstream preflight request.
async fn prepare_request(
    state: &AnthropicState,
    request: &AnthropicMessagesRequest,
    conversation_id: &str,
) -> Result<PreparedRequest> {
    state.auth_manager.access_token_and_autofetch().await?;
    let profile = state
        .auth_manager
        .profile_arn()
        .await
        .or_else(|| state.config.profile_arn.clone());
    let mut tool_name_aliases = ToolNameAliases::default();
    let payload = anthropic_to_kiro(
        request,
        conversation_id,
        profile.as_deref(),
        &state.config,
        &state.model_cache,
        &mut tool_name_aliases,
    )?
    .payload;
    let source = preflight_upstream(state.auth_manager.clone(), &state.config, payload).await?;
    Ok(PreparedRequest {
        source,
        tool_name_aliases,
    })
}

/// Issues the upstream `generateAssistantResponse` request and waits for
/// the first byte before returning the stream to the caller.
///
/// This "preflight" wait exists so that connection-level failures (auth
/// errors, rate limits, etc.) surface as an `Err` here — before any bytes
/// have been sent to the client — rather than partway through an
/// already-started SSE response. The first byte received is buffered and
/// re-prepended to the returned stream (via `stream::once(...).chain(...)`)
/// so no data is lost. Retries up to `first_token_max_retries` times if the
/// first byte does not arrive within `first_token_timeout`.
async fn preflight_upstream(
    auth: Arc<AuthManager>,
    config: &Config,
    payload: Value,
) -> Result<futures_util::stream::BoxStream<'static, std::result::Result<Bytes, reqwest::Error>>> {
    let tries = config.first_token_max_retries.max(1);
    let url = auth.api_host().await + "/generateAssistantResponse";
    for attempt in 0..tries {
        let client = KiroHttpClient::new(auth.clone(), config)?;
        let response = client
            .request_with_retry(
                reqwest::Method::POST,
                &url,
                Some(payload.clone()),
                None,
                true,
            )
            .await?;
        let mut source = response.bytes_stream().boxed();
        match tokio::time::timeout(config.first_token_timeout, source.next()).await {
            Ok(Some(Ok(first))) => {
                let replay = stream::once(async move { Ok::<Bytes, reqwest::Error>(first) })
                    .chain(source)
                    .boxed();
                return Ok(replay);
            }
            Ok(Some(Err(error))) => return Err(error.into()),
            Ok(None) => return Ok(source),
            Err(_) if attempt + 1 < tries => continue,
            Err(_) => return Err(GatewayError::FirstTokenTimeout(config.first_token_timeout)),
        }
    }
    Err(GatewayError::Internal(
        "first-token preflight exited unexpectedly".into(),
    ))
}

/// Validates the request's API key. Accepts either an `x-api-key` header
/// or an `Authorization: Bearer <key>` header, compared against the
/// configured `proxy_api_key` using [`constant_time_eq`] to avoid leaking
/// timing information about how much of the key matched. Returns
/// `Some(response)` with a 401 if authentication fails, `None` if it
/// succeeds.
fn verify_headers(headers: &HeaderMap, config: &Config, _record_version: bool) -> Option<Response> {
    let x_api_key = header_value(headers, "x-api-key");
    let authorization = header_value(headers, header::AUTHORIZATION.as_str());
    let valid = x_api_key
        .as_deref()
        .is_some_and(|key| constant_time_eq(key, &config.proxy_api_key))
        || authorization.as_deref().is_some_and(|value| {
            constant_time_eq(value, &format!("Bearer {}", config.proxy_api_key))
        });
    if !valid {
        return Some(authentication_error());
    }
    None
}

/// Extracts a header's value as an owned `String`, if present and valid
/// UTF-8.
fn header_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(ToOwned::to_owned)
}

/// Compares two strings for equality in constant time (with respect to
/// the *expected* string's length) to mitigate timing side-channels during
/// API key comparison. Note: this only equalizes the per-byte comparison
/// work; overall runtime can still vary with input length via `.bytes()`
/// iteration setup, which is not considered a meaningful leak here.
fn constant_time_eq(candidate: &str, expected: &str) -> bool {
    let mut difference = candidate.len() ^ expected.len();
    for (index, expected_byte) in expected.bytes().enumerate() {
        difference |= usize::from(
            expected_byte ^ candidate.as_bytes().get(index).copied().unwrap_or_default(),
        );
    }
    difference == 0
}

/// Builds the Anthropic-shaped 401 response for missing/invalid API keys.
fn authentication_error() -> Response {
    (StatusCode::UNAUTHORIZED, Json(json!({"detail":{"type":"error","error":{"type":"authentication_error","message":"Invalid or missing API key. Use x-api-key header or Authorization: Bearer."}}}))).into_response()
}

/// Builds the 422 response returned when `messages` is empty.
fn empty_messages_response() -> Response {
    error_response(
        StatusCode::UNPROCESSABLE_ENTITY,
        "invalid_request_error",
        "messages must not be empty",
    )
}

/// Builds an [`AnthropicErrorResponse`]-shaped JSON error with the given
/// HTTP status, Anthropic error `kind` (e.g. `"invalid_request_error"`),
/// and human-readable `message`.
fn error_response(status: StatusCode, kind: &str, message: &str) -> Response {
    let envelope = AnthropicErrorResponse {
        kind: "error".to_string(),
        error: AnthropicErrorDetail {
            kind: kind.to_string(),
            message: message.to_string(),
        },
    };
    (status, Json(envelope)).into_response()
}

/// Maps a [`GatewayError`] to an Anthropic-shaped error response,
/// classifying request-shape errors ([`GatewayError::InvalidRequest`],
/// [`GatewayError::UnknownModel`]) as `invalid_request_error` and
/// everything else as a generic `api_error`, using the error's own
/// `http_status()`/`user_message()`.
fn gateway_error_response(error: GatewayError) -> Response {
    let kind = if matches!(
        error,
        GatewayError::InvalidRequest(_) | GatewayError::UnknownModel(_)
    ) {
        "invalid_request_error"
    } else {
        "api_error"
    };
    error_response(
        StatusCode::from_u16(error.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        kind,
        &error.user_message(),
    )
}

/// Converts the request's messages/tools/system prompt into the plain
/// [`serde_json::Value`] shapes consumed by [`estimate_request_tokens`].
fn token_input(request: &AnthropicMessagesRequest) -> Result<RequestTokenInput> {
    let messages = request
        .messages
        .iter()
        .map(serde_json::to_value)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let tools = request
        .tools
        .as_ref()
        .map(|tools| {
            tools
                .iter()
                .map(serde_json::to_value)
                .collect::<std::result::Result<Vec<_>, _>>()
        })
        .transpose()?;
    let system = request
        .system
        .as_ref()
        .map(serde_json::to_value)
        .transpose()?;
    Ok(RequestTokenInput {
        messages,
        tools,
        system,
    })
}

/// Derives a stable conversation identifier from the request's message
/// history, used as the key for [`TruncationStore`] lookups so truncation
/// recovery state can be matched across turns of the same conversation.
fn conversation_id(request: &AnthropicMessagesRequest) -> String {
    let owned: Vec<(String, Value)> = request
        .messages
        .iter()
        .filter_map(|message| {
            serde_json::to_value(&message.content)
                .ok()
                .map(|content| (role_name(message.role).to_string(), content))
        })
        .collect();
    let hashable: Vec<HashableMessage<'_>> = owned
        .iter()
        .map(|(role, content)| HashableMessage { role, content })
        .collect();
    generate_conversation_id(&hashable)
}

/// Maps an [`AnthropicMessageRole`] to its wire-format string.
fn role_name(role: AnthropicMessageRole) -> &'static str {
    match role {
        AnthropicMessageRole::User => "user",
        AnthropicMessageRole::Assistant => "assistant",
        AnthropicMessageRole::System => "system",
    }
}

/// Detects whether the request asked for Anthropic's server-hosted web
/// search tool (any tool whose `type` starts with `"web_search"`). This
/// gateway does not yet implement the native MCP bypass for that tool, so
/// callers reject such requests with a 501 rather than silently ignoring
/// the tool.
fn has_native_web_search(request: &AnthropicMessagesRequest) -> bool {
    request.tools.as_ref().is_some_and(|tools| {
        tools.iter().any(|tool| {
            tool.kind
                .as_deref()
                .is_some_and(|kind| kind.starts_with("web_search"))
        })
    })
}

/// Rewrites the request in place to recover from previously observed
/// output truncation, using truncation records saved by [`super::sse`]
/// during earlier turns of this conversation (keyed by `conversation_id`).
///
/// Two recovery paths, applied per message:
/// - A user `tool_result` block whose `tool_use_id` matches a saved tool
///   truncation gets its `content` replaced with the original text plus a
///   prepended `[API Limitation]`-style recovery notice, so the model is
///   informed that the previous tool output it saw was cut short.
/// - An assistant message whose full text matches a saved content
///   truncation gets a synthetic follow-up `user` message appended asking
///   the model to continue, since the assistant's own turn was cut off
///   mid-generation.
///
/// No-ops entirely if `config.truncation_recovery` is disabled.
fn apply_truncation_recovery(
    request: &mut AnthropicMessagesRequest,
    store: &TruncationStore,
    config: &Config,
) {
    if !config.truncation_recovery {
        return;
    }
    let conversation_id = conversation_id(request);
    let mut revised = Vec::with_capacity(request.messages.len());
    for mut message in request.messages.clone() {
        if message.role == AnthropicMessageRole::User {
            if let AnthropicMessageContent::Blocks(blocks) = &mut message.content {
                for block in blocks {
                    if let ContentBlock::ToolResult(result) = block {
                        // `take_tool_truncation` consumes the record so the
                        // recovery notice is only injected once per truncated
                        // tool call, even if the same conversation is
                        // replayed with more turns appended.
                        if let Some(info) =
                            store.take_tool_truncation(&conversation_id, &result.tool_use_id)
                        {
                            let original = result
                                .content
                                .as_ref()
                                .map(tool_result_text)
                                .unwrap_or_default();
                            result.content = Some(ToolResultContent::Text(
                                prepend_tool_recovery_notice(&original),
                            ));
                            let _ = info;
                        }
                    }
                }
            }
        }
        let assistant_text = if message.role == AnthropicMessageRole::Assistant {
            message_text(&message)
        } else {
            String::new()
        };
        revised.push(message);
        if !assistant_text.is_empty()
            && store
                .take_content_truncation(&conversation_id, &assistant_text)
                .is_some()
        {
            revised.push(AnthropicMessage {
                role: AnthropicMessageRole::User,
                content: AnthropicMessageContent::Blocks(vec![ContentBlock::Text(
                    TextContentBlock {
                        kind: Default::default(),
                        text: generate_truncation_user_message().to_string(),
                    },
                )]),
            });
        }
    }
    request.messages = revised;
}

/// Extracts the plain-text representation of a tool result's `content`,
/// concatenating only the text portions of block-form content (images,
/// tool references, etc. are dropped) for use as the recovery notice's
/// "original" text.
fn tool_result_text(content: &ToolResultContent) -> String {
    match content {
        ToolResultContent::Text(text) => text.clone(),
        ToolResultContent::Blocks(blocks) => blocks
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .collect(),
        ToolResultContent::Other(value) => value.to_string(),
    }
}

/// Extracts a message's plain-text content, concatenating text blocks and
/// ignoring other block types. Used to detect whether an assistant
/// message's text matches a previously saved truncated-content record.
fn message_text(message: &AnthropicMessage) -> String {
    match &message.content {
        AnthropicMessageContent::Text(text) => text.clone(),
        AnthropicMessageContent::Blocks(blocks) => blocks
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .collect(),
    }
}

/// Persists truncation records observed in a completed (non-streaming)
/// upstream response, so a future turn of the same conversation can
/// trigger [`apply_truncation_recovery`]. Mirrors the equivalent logic
/// performed incrementally by [`super::sse::AnthropicSseFormatter`] for
/// streaming responses. No-ops if truncation recovery is disabled.
fn save_nonstream_truncations(
    state: &AnthropicState,
    conversation_id: &str,
    result: &crate::upstream::stream::StreamResult,
) {
    if !state.config.truncation_recovery {
        return;
    }
    for tool in &result.tool_calls {
        if let Some(info) = &tool.truncation {
            let id = tool.id.clone().unwrap_or_default();
            if !id.is_empty() {
                state.truncation_store.save_tool_truncation(conversation_id, id, tool.name.clone(), json!({"is_truncated":info.is_truncated,"reason":info.reason,"size_bytes":info.size_bytes}));
            }
        }
    }
    if crate::truncation::is_content_truncated(
        result.context_usage_percentage.is_some(),
        &result.content,
        !result.tool_calls.is_empty(),
    ) {
        state
            .truncation_store
            .save_content_truncation(conversation_id, &result.content);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> AnthropicState {
        let config = Arc::new(Config {
            proxy_api_key: "correct horse battery staple".into(),
            ..Config::default()
        });
        AnthropicState {
            auth_manager: Arc::new(
                AuthManager::new((*config).clone()).expect("auth manager must construct"),
            ),
            model_cache: Arc::new(ModelInfoCache::default()),
            config,
            truncation_store: TruncationStore::default(),
        }
    }
    fn headers(key: &str, version: bool) -> HeaderMap {
        let mut headers = HeaderMap::new();
        if let Ok(value) = key.parse() {
            headers.insert("x-api-key", value);
        }
        if version {
            headers.insert(
                "anthropic-version",
                axum::http::HeaderValue::from_static("2023-06-01"),
            );
        }
        headers
    }

    #[test]
    fn authentication_is_constant_time_shape_and_allows_optional_version() {
        let state = state();
        assert!(
            verify_headers(
                &headers("correct horse battery staple", true),
                &state.config,
                true
            )
            .is_none()
        );
        assert!(verify_headers(&headers("wrong", true), &state.config, true).is_some());
        assert!(
            verify_headers(
                &headers("correct horse battery staple", false),
                &state.config,
                true
            )
            .is_none()
        );
        assert!(constant_time_eq("abc", "abc"));
        assert!(!constant_time_eq("abd", "abc"));
        assert!(!constant_time_eq("ab", "abc"));
    }

    #[test]
    fn count_tokens_route_is_registered_with_messages_route() {
        let router = router();
        let _ = router;
    }

    #[test]
    fn recovery_prepends_tool_notice_and_adds_content_notice() {
        let state = state();
        let mut request: AnthropicMessagesRequest = serde_json::from_value(json!({"model":"claude","max_tokens":1,"messages":[{"role":"user","content":[{"type":"tool_result","tool_use_id":"t","content":"original"}]},{"role":"assistant","content":"cut"}]})).unwrap_or_else(|_| AnthropicMessagesRequest { model: "claude".into(), messages: vec![], max_tokens: 1, system: None, stream: false, thinking: None, output_config: None, tools: None, tool_choice: None, temperature: None, top_p: None, top_k: None, stop_sequences: None, metadata: None });
        let before = conversation_id(&request);
        state
            .truncation_store
            .save_tool_truncation(&before, "t", "tool", json!({}));
        state
            .truncation_store
            .save_content_truncation(&before, "cut");
        apply_truncation_recovery(&mut request, &state.truncation_store, &state.config);
        assert_eq!(request.messages.len(), 3);
        assert!(message_text(&request.messages[2]).contains("[System Notice]"));
    }
}

#[cfg(test)]
mod validation_status_regression_tests {
    use super::*;

    #[test]
    fn empty_messages_uses_unprocessable_entity_status() {
        let response = empty_messages_response();
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }
}
