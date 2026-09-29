//! SSE encoding for the OpenAI Chat Completions API.
//!
//! Converts Kiro's internal event stream ([`crate::upstream::KiroEvent`])
//! into `chat.completion.chunk` Server-Sent Events frames, and separately
//! assembles the equivalent single `chat.completion` JSON response for
//! non-streaming requests. Both paths restore original (pre-alias) tool
//! names via [`crate::compat::ToolNameAliases`] and record truncation
//! observations via [`crate::truncation`] for later recovery.
//!
//! Unlike the Anthropic formatter (which is a stateful struct fed one
//! event at a time), [`encode_openai_sse`] is a single `async_stream`
//! generator that consumes the whole upstream event stream and yields SSE
//! frame strings, since OpenAI's chunk format needs less
//! block-open/close bookkeeping than Anthropic's indexed content blocks.

use std::time::{SystemTime, UNIX_EPOCH};

use async_stream::try_stream;
use futures_util::{Stream, StreamExt};
use serde_json::{Map, Value, json};

use crate::compat::ToolNameAliases;
use crate::config::Config;
use crate::error::Result;
use crate::model::ModelInfoCache;
use crate::tokenizer::{
    calculate_tokens_from_context_usage, count_message_tokens, count_tokens, count_tools_tokens,
};
use crate::truncation::{TruncationStore, is_content_truncated};
use crate::upstream::{
    KiroEvent, KiroEventType, StreamResult, ToolCall, collect_stream_to_result,
    deduplicate_tool_calls, parse_bracket_tool_calls,
};
use crate::utils::{format_json_spaced, generate_completion_id};

/// Per-request context shared by both the streaming ([`encode_openai_sse`])
/// and non-streaming ([`collect_openai_response`]) response paths: the
/// resolved model name/cache for token accounting, the original request
/// messages/tools (for prompt-token fallback estimation), gateway config,
/// the conversation id used as the truncation-store key, and the
/// per-request tool name alias table.
#[derive(Clone)]
pub struct OpenAiFormatContext {
    /// The client-facing model id being served.
    pub model: String,
    /// Cached model catalog, used for context-window/reasoning lookups.
    pub model_cache: ModelInfoCache,
    /// The original request's messages, for prompt-token fallback
    /// estimation.
    pub request_messages: Vec<Value>,
    /// The original request's tool definitions, for prompt-token fallback
    /// estimation.
    pub request_tools: Option<Vec<Value>>,
    /// Effective gateway configuration.
    pub config: Config,
    /// Conversation id used as the truncation-store key.
    pub conversation_id: String,
    /// Recovery records for upstream tool-call/content truncation.
    pub truncation_store: TruncationStore,
    /// Per-request tool name alias table (see [`ToolNameAliases`]).
    pub tool_name_aliases: ToolNameAliases,
}

impl OpenAiFormatContext {
    /// Creates a new formatting context with default (empty) tool name
    /// aliases; call [`Self::with_tool_name_aliases`] to attach the
    /// request-scoped alias table produced during payload conversion.
    ///
    /// # Examples
    ///
    /// ```
    /// use lanius_core::api::OpenAiFormatContext;
    /// use lanius_core::model::ModelInfoCache;
    /// use lanius_core::truncation::TruncationStore;
    /// use lanius_core::Config;
    ///
    /// let context = OpenAiFormatContext::new(
    ///     "claude-sonnet-4.5",
    ///     ModelInfoCache::default(),
    ///     vec![],
    ///     None,
    ///     Config::default(),
    ///     "conv-1",
    ///     TruncationStore::default(),
    /// );
    /// assert_eq!(context.model, "claude-sonnet-4.5");
    /// ```
    pub fn new(
        model: impl Into<String>,
        model_cache: ModelInfoCache,
        request_messages: Vec<Value>,
        request_tools: Option<Vec<Value>>,
        config: Config,
        conversation_id: impl Into<String>,
        truncation_store: TruncationStore,
    ) -> Self {
        Self {
            model: model.into(),
            model_cache,
            request_messages,
            request_tools,
            config,
            conversation_id: conversation_id.into(),
            truncation_store,
            tool_name_aliases: ToolNameAliases::default(),
        }
    }

    /// Attaches the per-request tool name alias table so tool
    /// names/mentions are restored to their original form before reaching
    /// the client.
    ///
    /// # Examples
    ///
    /// ```
    /// use lanius_core::api::OpenAiFormatContext;
    /// use lanius_core::compat::ToolNameAliases;
    /// use lanius_core::model::ModelInfoCache;
    /// use lanius_core::truncation::TruncationStore;
    /// use lanius_core::Config;
    ///
    /// let context = OpenAiFormatContext::new(
    ///     "claude-sonnet-4.5", ModelInfoCache::default(), vec![], None,
    ///     Config::default(), "conv-1", TruncationStore::default(),
    /// ).with_tool_name_aliases(ToolNameAliases::default());
    /// ```
    pub fn with_tool_name_aliases(mut self, aliases: ToolNameAliases) -> Self {
        self.tool_name_aliases = aliases;
        self
    }
}

/// Encodes a stream of parsed upstream Kiro events into OpenAI
/// `chat.completion.chunk` SSE frames.
///
/// Frame sequence emitted: an optional leading `role: "assistant"` delta
/// (attached to whichever is the first content/thinking/tool-call chunk,
/// rather than sent as its own separate frame — matching how OpenAI
/// clients typically expect a single combined first delta), zero or more
/// content/reasoning-content deltas, an optional single `tool_calls` delta
/// carrying all accumulated tool calls at once, a final delta carrying
/// `finish_reason` and usage, and a terminating `data: [DONE]` frame.
/// Bracket-style tool calls embedded in the text (see
/// [`parse_bracket_tool_calls`]) are extracted and merged with any
/// natively reported tool calls, then deduplicated
/// ([`deduplicate_tool_calls`]) before being emitted.
///
/// # Examples
///
/// ```
/// use lanius_core::api::{encode_openai_sse, OpenAiFormatContext};
/// use lanius_core::model::ModelInfoCache;
/// use lanius_core::truncation::TruncationStore;
/// use lanius_core::upstream::KiroEvent;
/// use lanius_core::Config;
/// use futures_util::StreamExt;
///
/// # async fn example() {
/// let context = OpenAiFormatContext::new(
///     "claude-sonnet-4.5", ModelInfoCache::default(), vec![], None,
///     Config::default(), "conv-1", TruncationStore::default(),
/// );
/// let events = futures_util::stream::iter(vec![Ok(KiroEvent::content("hi"))]);
/// let frames: Vec<_> = encode_openai_sse(events, context).collect().await;
/// assert!(!frames.is_empty());
/// # }
/// ```
pub fn encode_openai_sse<S>(
    events: S,
    context: OpenAiFormatContext,
) -> impl Stream<Item = Result<String>> + Send
where
    S: Stream<Item = Result<KiroEvent>> + Send + 'static,
{
    try_stream! {
        let completion_id = generate_completion_id();
        let created = unix_timestamp();
        let mut first_chunk = true;
        let mut full_content = String::new();
        let mut alias_text_pending = String::new();
        let mut full_thinking_content = String::new();
        let mut tool_calls = Vec::new();
        let mut metering_data = None;
        let mut received_usage = false;
        let mut context_usage = None;
        futures_util::pin_mut!(events);

        while let Some(event) = events.next().await {
            let event = match event {
                Ok(event) => event,
                Err(error) => {
                    // A mid-stream upstream failure (read timeout, connection
                    // reset, ...). Surface it as an OpenAI-style error frame
                    // instead of silently ending the stream, which clients
                    // report as "stream ended without finish_reason".
                    tracing::warn!(error = %error, "upstream stream failed mid-response");
                    let trailing_content = context
                        .tool_name_aliases
                        .restore_text_fragment(&mut alias_text_pending, "", true);
                    if !trailing_content.is_empty() {
                        let mut delta = Map::new();
                        delta.insert("content".to_string(), Value::String(trailing_content));
                        if first_chunk {
                            delta.insert("role".to_string(), Value::String("assistant".to_string()));
                        }
                        yield frame(&chunk_value(&completion_id, created, &context.model, Value::Object(delta), None, None));
                    }
                    yield frame(&error_value(&error));
                    yield "data: [DONE]\n\n".to_string();
                    return;
                }
            };
            match event {
                KiroEvent { event_type: KiroEventType::Content, content: Some(content), .. } if !content.is_empty() => {
                    full_content.push_str(&content);
                    let mut delta = Map::new();
                    // Buffer any partial tool-name alias fragment so a
                    // client never sees a half-restored alias split across
                    // two upstream chunks.
                    let client_content = context.tool_name_aliases.restore_text_fragment(
                        &mut alias_text_pending,
                        &content,
                        false,
                    );
                    if client_content.is_empty() {
                        continue;
                    }
                    delta.insert("content".to_string(), Value::String(client_content));
                    if first_chunk {
                        delta.insert("role".to_string(), Value::String("assistant".to_string()));
                        first_chunk = false;
                    }
                    yield frame(&chunk_value(&completion_id, created, &context.model, Value::Object(delta), None, None));
                }
                KiroEvent { event_type: KiroEventType::Thinking, thinking_content: Some(thinking), .. } if !thinking.is_empty() => {
                    full_thinking_content.push_str(&thinking);
                    let mut delta = Map::new();
                    // Native thinking is always surfaced in the dedicated
                    // `reasoning_content` field, never mixed into `content`.
                    delta.insert("reasoning_content".to_string(), Value::String(thinking));
                    if first_chunk {
                        delta.insert("role".to_string(), Value::String("assistant".to_string()));
                        first_chunk = false;
                    }
                    yield frame(&chunk_value(&completion_id, created, &context.model, Value::Object(delta), None, None));
                }
                KiroEvent { event_type: KiroEventType::ToolUse, tool_use: Some(tool), .. } => tool_calls.push(tool),
                KiroEvent { event_type: KiroEventType::Usage, usage, .. } => {
                    received_usage = usage.is_some();
                    metering_data = usage;
                }
                KiroEvent { event_type: KiroEventType::ContextUsage, context_usage_percentage: Some(percentage), .. } => {
                    context_usage = Some(percentage);
                }
                _ => {}
            }
        }

        // Flush any tool-name-alias fragment still held back at end of
        // stream, since no further content chunks are coming to complete it.
        let trailing_content = context
            .tool_name_aliases
            .restore_text_fragment(&mut alias_text_pending, "", true);
        if !trailing_content.is_empty() {
            let mut delta = Map::new();
            delta.insert("content".to_string(), Value::String(trailing_content));
            if first_chunk {
                delta.insert("role".to_string(), Value::String("assistant".to_string()));
            }
            yield frame(&chunk_value(&completion_id, created, &context.model, Value::Object(delta), None, None));
        }

        let bracket_calls = parse_bracket_tool_calls(&full_content);
        tool_calls.extend(bracket_calls);
        let tool_calls = deduplicate_tool_calls(tool_calls);
        let content_truncated = is_content_truncated(
            received_usage || context_usage.is_some(),
            &full_content,
            !tool_calls.is_empty(),
        );
        save_truncations(&context, &tool_calls, content_truncated, &full_content);

        if !tool_calls.is_empty() {
            let indexed_calls: Vec<Value> = tool_calls
                .iter()
                .enumerate()
                .map(|(index, call)| tool_call_value(index, call, &context.tool_name_aliases))
                .collect();
            yield frame(&chunk_value(
                &completion_id,
                created,
                &context.model,
                json!({"tool_calls": indexed_calls}),
                None,
                None,
            ));
        }

        let finish_reason = if content_truncated {
            "length"
        } else if tool_calls.is_empty() {
            "stop"
        } else {
            "tool_calls"
        };
        let usage = usage_value(
            &context,
            &full_content,
            &full_thinking_content,
            context_usage,
            metering_data.as_ref(),
        );
        yield frame(&chunk_value(
            &completion_id,
            created,
            &context.model,
            Value::Object(Map::new()),
            Some(finish_reason),
            Some(usage),
        ));
        yield "data: [DONE]\n\n".to_string();
    }
}

/// Collects a raw upstream byte stream into a single `chat.completion`
/// JSON response body, for non-streaming requests.
///
/// Buffers the entire response via [`collect_stream_to_result`] (applying
/// the same first-token/read timeouts as the streaming path) before delegating to
/// `response_value_from_result`.
///
/// # Examples
///
/// ```
/// use lanius_core::api::{collect_openai_response, OpenAiFormatContext};
/// use lanius_core::model::ModelInfoCache;
/// use lanius_core::truncation::TruncationStore;
/// use lanius_core::Config;
///
/// # async fn example() {
/// let context = OpenAiFormatContext::new(
///     "claude-sonnet-4.5", ModelInfoCache::default(), vec![], None,
///     Config::default(), "conv-1", TruncationStore::default(),
/// );
/// let chunks = vec![Ok(bytes::Bytes::from(r#"{"content":"hi"}"#))];
/// let stream = futures_util::stream::iter(chunks);
/// let response = collect_openai_response(stream, context).await.expect("valid response");
/// assert!(response["choices"].is_array());
/// # }
/// ```
pub async fn collect_openai_response<S>(
    byte_stream: S,
    context: OpenAiFormatContext,
) -> Result<Value>
where
    S: Stream<Item = std::result::Result<bytes::Bytes, reqwest::Error>> + Send + Unpin + 'static,
{
    let result = collect_stream_to_result(
        byte_stream,
        context.config.first_token_timeout,
        context.config.streaming_read_timeout,
    )
    .await?;
    response_value_from_result(result, &context)
}

/// Builds the non-streaming `chat.completion` response body from a
/// collected [`StreamResult`]: restores original tool names, detects
/// truncation and saves a recovery record if needed, and assembles the
/// `message` object (optionally including `reasoning_content` and
/// `tool_calls`) along with computed token usage.
fn response_value_from_result(
    mut result: StreamResult,
    context: &OpenAiFormatContext,
) -> Result<Value> {
    result.content = context.tool_name_aliases.restore_text(&result.content);
    for tool in &mut result.tool_calls {
        tool.name = context.tool_name_aliases.original_for(&tool.name);
    }
    let content_truncated = is_content_truncated(
        result.usage.is_some() || result.context_usage_percentage.is_some(),
        &result.content,
        !result.tool_calls.is_empty(),
    );
    save_truncations(
        context,
        &result.tool_calls,
        content_truncated,
        &result.content,
    );
    let finish_reason = if content_truncated {
        "length"
    } else if result.tool_calls.is_empty() {
        "stop"
    } else {
        "tool_calls"
    };
    let usage = usage_value(
        context,
        &result.content,
        &result.thinking_content,
        result.context_usage_percentage,
        result.usage.as_ref(),
    );
    let mut message = Map::new();
    message.insert("role".to_string(), Value::String("assistant".to_string()));
    message.insert("content".to_string(), Value::String(result.content));
    if !result.thinking_content.is_empty() {
        message.insert(
            "reasoning_content".to_string(),
            Value::String(result.thinking_content),
        );
    }
    if !result.tool_calls.is_empty() {
        message.insert(
            "tool_calls".to_string(),
            Value::Array(
                result
                    .tool_calls
                    .iter()
                    .map(non_stream_tool_call_value)
                    .collect(),
            ),
        );
    }
    Ok(json!({
        "id": generate_completion_id(),
        "object": "chat.completion",
        "created": unix_timestamp(),
        "model": context.model,
        "choices": [{"index": 0, "message": Value::Object(message), "finish_reason": finish_reason}],
        "usage": usage,
    }))
}

/// Builds a single `chat.completion.chunk` JSON payload.
fn chunk_value(
    id: &str,
    created: i64,
    model: &str,
    delta: Value,
    finish_reason: Option<&str>,
    usage: Option<Value>,
) -> Value {
    let mut value = json!({
        "id": id,
        "object": "chat.completion.chunk",
        "created": created,
        "model": model,
        "choices": [{"index": 0, "delta": Value::Null, "finish_reason": finish_reason}],
    });
    value["choices"][0]["delta"] = delta;
    if let Some(usage) = usage {
        value["usage"] = usage;
    }
    value
}

/// Builds an OpenAI-style mid-stream error payload
/// (`{"error": {"message", "type", "code"}}`), using the error's
/// user-safe message.
fn error_value(error: &crate::error::GatewayError) -> Value {
    json!({
        "error": {
            "message": error.user_message(),
            "type": "kiro_stream_error",
            "code": error.http_status(),
        }
    })
}

/// Formats a JSON value as a `data: ...\n\n` SSE frame using spaced JSON
/// serialization (see [`format_json_spaced`]).
fn frame(value: &Value) -> String {
    format!("data: {}\n\n", format_json_spaced(value))
}

/// Builds a single indexed tool-call entry for a streaming `tool_calls`
/// delta, restoring the tool's original (pre-alias) name.
fn tool_call_value(index: usize, call: &ToolCall, aliases: &ToolNameAliases) -> Value {
    json!({
        "index": index,
        "id": call.id,
        "type": "function",
        "function": {"name": aliases.original_for(&call.name), "arguments": call.arguments},
    })
}

/// Builds a single tool-call entry for the non-streaming response's
/// `message.tool_calls` array. Unlike [`tool_call_value`] this has no
/// `index` field (OpenAI's non-streaming shape omits it) and the caller
/// is expected to have already restored the original tool name on `call`.
fn non_stream_tool_call_value(call: &ToolCall) -> Value {
    json!({
        "id": call.id,
        "type": "function",
        "function": {"name": call.name, "arguments": call.arguments},
    })
}

/// Computes the `usage` object for a response: `completion_tokens` from
/// locally counting the generated text, `prompt_tokens` preferring the
/// upstream's context-usage-percentage-derived estimate and falling back
/// to locally counting the request messages/tools when the upstream did
/// not report usable context usage. Includes a Kiro-specific
/// `credits_used` field when the upstream metering data is "truthy" (see
/// [`is_truthy_value`]) — an empty metering object `{}` is treated as
/// absent rather than as a real (if empty) usage record — plus
/// `prompt_tokens_details.cached_tokens` when metering reports cache reads.
fn usage_value(
    context: &OpenAiFormatContext,
    content: &str,
    thinking: &str,
    context_usage: Option<f64>,
    metering_data: Option<&Value>,
) -> Value {
    let completion_tokens = count_tokens(&format!("{content}{thinking}"), true);
    let mut calculated = calculate_tokens_from_context_usage(
        context_usage,
        completion_tokens,
        &context.model_cache,
        &context.model,
    );
    if calculated.prompt_source == "unknown" && !context.request_messages.is_empty() {
        calculated.prompt_tokens = count_message_tokens(&context.request_messages, false)
            .saturating_add(count_tools_tokens(context.request_tools.as_deref(), false));
        calculated.total_tokens = calculated.prompt_tokens.saturating_add(completion_tokens);
    }
    let mut usage = Map::new();
    usage.insert("prompt_tokens".to_string(), json!(calculated.prompt_tokens));
    usage.insert("completion_tokens".to_string(), json!(completion_tokens));
    usage.insert("total_tokens".to_string(), json!(calculated.total_tokens));
    if let Some(metering) = metering_data.filter(|value| is_truthy_value(value)) {
        // A `meteringEvent` object carries the credit amount under `usage`;
        // any other shape is passed through as-is.
        let credits = metering
            .get("usage")
            .filter(|value| value.is_number())
            .unwrap_or(metering);
        usage.insert("credits_used".to_string(), credits.clone());
        if let Some(cached) = cached_prompt_tokens(metering) {
            usage.insert(
                "prompt_tokens_details".to_string(),
                json!({"cached_tokens": cached}),
            );
        }
    }
    Value::Object(usage)
}

/// Reads the upstream prompt-cache read count (either key spelling) for
/// OpenAI's `prompt_tokens_details.cached_tokens`.
fn cached_prompt_tokens(metering: &Value) -> Option<i64> {
    ["cache_read_input_tokens", "cacheReadInputTokens"]
        .iter()
        .find_map(|key| metering.get(*key).and_then(Value::as_i64))
}

/// Persists truncation records for tool calls and/or content observed in
/// this response, so a later turn of the same conversation can trigger
/// recovery (see `routes::inject_truncation_recovery`). No-ops if
/// truncation recovery is disabled.
fn save_truncations(
    context: &OpenAiFormatContext,
    tool_calls: &[ToolCall],
    content_truncated: bool,
    content: &str,
) {
    if !context.config.truncation_recovery {
        return;
    }
    for tool in tool_calls {
        if let Some(info) = &tool.truncation {
            if let Some(id) = tool.id.as_deref() {
                context.truncation_store.save_tool_truncation(
                    &context.conversation_id,
                    id,
                    &tool.name,
                    json!({
                        "is_truncated": info.is_truncated,
                        "reason": info.reason,
                        "size_bytes": info.size_bytes,
                    }),
                );
            }
        }
    }
    if content_truncated {
        context
            .truncation_store
            .save_content_truncation(&context.conversation_id, content);
    }
}

/// Evaluates a JSON value's truthiness following the same falsy convention
/// Kiro's own JSON-based protocol uses (empty string/array/object/`0` are
/// falsy) — notably an empty JSON object `{}` is considered falsy here,
/// unlike a naive "is this Some" check.
fn is_truthy_value(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(number) => number.as_f64() != Some(0.0),
        Value::String(value) => !value.is_empty(),
        Value::Array(value) => !value.is_empty(),
        Value::Object(value) => !value.is_empty(),
    }
}

/// Current Unix timestamp in seconds, used for the `created` field of
/// completion responses/chunks.
fn unix_timestamp() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs() as i64)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use futures_util::{StreamExt, stream};
    use serde_json::json;

    use super::*;

    fn context() -> OpenAiFormatContext {
        let cache = ModelInfoCache::default();
        cache.update(vec![
            json!({"modelId":"m", "tokenLimits":{"maxInputTokens":1000}}),
        ]);
        let config = Config::default();
        OpenAiFormatContext::new(
            "m",
            cache,
            vec![json!({"role":"user", "content":"hello"})],
            None,
            config,
            "conversation",
            TruncationStore::default(),
        )
    }

    async fn frames(events: Vec<Result<KiroEvent>>) -> Vec<String> {
        encode_openai_sse(stream::iter(events), context())
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<Result<Vec<_>>>()
            .expect("fixture stream must encode")
    }

    fn json_frame(frame: &str) -> Value {
        serde_json::from_str(
            frame
                .strip_prefix("data: ")
                .and_then(|value| value.strip_suffix("\n\n"))
                .expect("JSON frame"),
        )
        .expect("valid JSON frame")
    }

    #[tokio::test]
    async fn exact_frames_have_order_role_reasoning_tools_final_and_done() {
        let tool = ToolCall {
            id: Some("call_1".into()),
            name: "weather".into(),
            arguments: r#"{"city": "北京"}"#.into(),
            truncation: None,
        };
        let output = frames(vec![
            Ok(KiroEvent::thinking(Some("reason".into()), None)),
            Ok(KiroEvent::thinking(None, Some("sig".into()))),
            Ok(KiroEvent::content("answer")),
            Ok(KiroEvent::tool_use(tool)),
            Ok(KiroEvent::usage(json!({"credits": 1}))),
        ])
        .await;
        assert_eq!(output.len(), 5);
        assert!(output.iter().all(|frame| frame == "data: [DONE]\n\n"
            || (frame.starts_with("data: ") && frame.ends_with("\n\n"))));
        assert_eq!(output.last().map(String::as_str), Some("data: [DONE]\n\n"));
        let first = json_frame(&output[0]);
        assert_eq!(first["choices"][0]["delta"]["reasoning_content"], "reason");
        assert_eq!(first["choices"][0]["delta"]["role"], "assistant");
        assert_eq!(
            json_frame(&output[1])["choices"][0]["delta"]["content"],
            "answer"
        );
        let calls = &json_frame(&output[2])["choices"][0]["delta"]["tool_calls"];
        assert_eq!(calls[0]["index"], 0);
        assert_eq!(calls[0]["function"]["arguments"], r#"{"city": "北京"}"#);
        let final_frame = json_frame(&output[3]);
        assert_eq!(final_frame["choices"][0]["finish_reason"], "tool_calls");
        assert_eq!(final_frame["usage"]["credits_used"], json!({"credits": 1}));
    }

    #[tokio::test]
    async fn metering_event_maps_to_credits_and_cached_tokens() {
        let output = frames(vec![
            Ok(KiroEvent::content("answer")),
            Ok(KiroEvent::usage(json!({
                "unit": "credit",
                "unitPlural": "credits",
                "usage": 0.03,
                "cacheReadInputTokens": 1200,
            }))),
        ])
        .await;
        let final_frame = json_frame(&output[output.len() - 2]);
        assert_eq!(final_frame["usage"]["credits_used"], json!(0.03));
        assert_eq!(
            final_frame["usage"]["prompt_tokens_details"]["cached_tokens"],
            json!(1200)
        );
    }

    #[tokio::test]
    async fn metering_without_cache_fields_omits_prompt_tokens_details() {
        let output = frames(vec![
            Ok(KiroEvent::content("answer")),
            Ok(KiroEvent::usage(json!({"unit": "credit", "usage": 0.01}))),
        ])
        .await;
        let final_frame = json_frame(&output[output.len() - 2]);
        assert_eq!(final_frame["usage"]["credits_used"], json!(0.01));
        assert_eq!(final_frame["usage"].get("prompt_tokens_details"), None);
    }

    #[tokio::test]
    async fn tool_arguments_are_one_eof_frame_and_name_is_never_top_level_fallback() {
        let output = frames(vec![
            Ok(KiroEvent::tool_use(ToolCall {
                id: Some("a".into()),
                name: "".into(),
                arguments: "{\"x\": 1}".into(),
                truncation: None,
            })),
            Ok(KiroEvent::tool_use(ToolCall {
                id: Some("b".into()),
                name: "next".into(),
                arguments: "{}".into(),
                truncation: None,
            })),
            Ok(KiroEvent::usage(json!({}))),
        ])
        .await;
        let tool_frames: Vec<_> = output
            .iter()
            .filter(|frame| frame.contains("\"tool_calls\":"))
            .collect();
        assert_eq!(tool_frames.len(), 1);
        let calls = &json_frame(tool_frames[0])["choices"][0]["delta"]["tool_calls"];
        assert_eq!(calls[0]["index"], 0);
        assert_eq!(calls[0]["function"]["name"], "");
        assert_eq!(calls[1]["index"], 1);
        assert_eq!(json_frame(&output[1])["usage"].get("credits_used"), None);
    }

    #[tokio::test]
    async fn missing_completion_signal_is_length_and_empty_stream_still_ends_done() {
        let truncated = frames(vec![Ok(KiroEvent::content("partial"))]).await;
        assert_eq!(
            json_frame(&truncated[1])["choices"][0]["finish_reason"],
            "length"
        );
        let empty = frames(Vec::new()).await;
        assert_eq!(empty.len(), 2);
        assert_eq!(empty[1], "data: [DONE]\n\n");
    }

    #[tokio::test]
    async fn collector_uses_shared_upstream_result_path() {
        let bytes = stream::iter(vec![Ok(bytes::Bytes::from_static(
            br#"{"content":"hello"}{"usage":{"x":1}}"#,
        ))]);
        let response = collect_openai_response(bytes, context())
            .await
            .expect("collect response");
        assert_eq!(response["choices"][0]["message"]["content"], "hello");
        assert_eq!(response["choices"][0]["finish_reason"], "stop");
    }

    #[tokio::test]
    async fn mid_stream_error_emits_error_frame_before_done() {
        let output = frames(vec![
            Ok(KiroEvent::content("partial")),
            Err(crate::error::GatewayError::StreamReadTimeout(
                Duration::from_secs(1),
            )),
        ])
        .await;
        assert_eq!(output.len(), 3);
        assert_eq!(
            json_frame(&output[0])["choices"][0]["delta"]["content"],
            "partial"
        );
        let error = json_frame(&output[1]);
        assert_eq!(error["error"]["code"], 504);
        assert!(error["error"]["message"].is_string());
        assert_eq!(output[2], "data: [DONE]\n\n");
    }

    #[test]
    fn truthiness_matches_empty_usage_quirk() {
        assert!(!is_truthy_value(&json!({})));
        assert!(is_truthy_value(&json!({"credits": 0})));
        assert!(!is_truthy_value(&json!(0)));
        assert!(is_truthy_value(&json!(1)));
        assert_eq!(Duration::from_secs(1), Duration::from_secs(1));
    }
    #[tokio::test]
    async fn request_scoped_aliases_round_trip_openai_conversion_sse_and_collection() {
        let original = "mcp__plugin_everything_claude_code_github__create_pull_request_review";
        let request: crate::api::ChatCompletionRequest = serde_json::from_value(json!({
            "model":"claude", "messages":[{"role":"user","content":"use the tool"}],
            "tools":[{"type":"function","function":{"name":original,"parameters":{"type":"object"}}}]
        })).unwrap_or_else(|error| panic!("fixture request must deserialize: {error}"));
        let mut aliases = ToolNameAliases::default();
        let payload = crate::convert::build_kiro_payload(
            &request,
            "conversation",
            None,
            &Config::default(),
            &ModelInfoCache::default(),
            &mut aliases,
        )
        .unwrap_or_else(|error| panic!("payload conversion must succeed: {error}"))
        .payload;
        let alias = aliases.alias_for(original);
        assert_ne!(alias, original);
        assert!(alias.len() <= crate::compat::MAX_KIRO_TOOL_NAME_LENGTH);
        assert!(payload.to_string().contains(&alias));

        let context = context().with_tool_name_aliases(aliases.clone());
        let split_at = alias.chars().count() / 2;
        let alias_prefix: String = alias.chars().take(split_at).collect();
        let alias_suffix: String = alias.chars().skip(split_at).collect();
        let wire = encode_openai_sse(
            stream::iter(vec![
                Ok(KiroEvent::content(format!("[Called {alias_prefix}"))),
                Ok(KiroEvent::content(format!(
                    "{alias_suffix} with args: {{}}]"
                ))),
                Ok(KiroEvent::tool_use(ToolCall {
                    id: Some("call".into()),
                    name: alias.clone(),
                    arguments: "{}".into(),
                    truncation: None,
                })),
            ]),
            context.clone(),
        )
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<Vec<_>>>()
        .unwrap_or_else(|error| panic!("SSE must encode: {error}"))
        .join("");
        assert!(wire.contains(original));
        assert!(!wire.contains(&format!("[Called {alias} with args:")));

        let collected = response_value_from_result(
            StreamResult {
                content: format!("[Called {alias} with args: {{}}]"),
                tool_calls: vec![ToolCall {
                    id: Some("call".into()),
                    name: alias,
                    arguments: "{}".into(),
                    truncation: None,
                }],
                ..StreamResult::default()
            },
            &context,
        )
        .unwrap_or_else(|error| panic!("aggregation must succeed: {error}"));
        assert_eq!(
            collected["choices"][0]["message"]["tool_calls"][0]["function"]["name"],
            original
        );
        assert!(
            collected["choices"][0]["message"]["content"]
                .as_str()
                .is_some_and(|text| text.contains(original))
        );
    }
}
