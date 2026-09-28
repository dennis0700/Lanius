//! SSE encoding for the Anthropic Messages API.
//!
//! Converts Kiro's internal event stream ([`crate::upstream::stream`])
//! into the sequence of Server-Sent Events frames expected by Anthropic
//! clients (`message_start`, `content_block_start/delta/stop`,
//! `message_delta`, `message_stop`, `ping`, `error`), and also assembles
//! the equivalent single JSON response for non-streaming requests.
//!
//! The core abstraction is [`AnthropicSseFormatter`], a state machine that
//! tracks which content block (text/thinking/tool-use) is currently open
//! so it can emit matching `content_block_start`/`content_block_stop`
//! pairs at the right index as Kiro events arrive. It also accumulates
//! full content/thinking text for later token counting and truncation
//! detection (see [`crate::truncation`]).

use serde::Serialize;
use serde_json::{Map, Value, json};

use crate::compat::ToolNameAliases;
use crate::error::Result;
use crate::model::ModelInfoCache;
use crate::tokenizer::{
    calculate_tokens_from_context_usage, count_tokens, estimate_request_tokens,
};
use crate::truncation::{TruncationStore, is_content_truncated};
use crate::upstream::{KiroEvent, KiroEventType, StreamResult, ToolCall, parse_bracket_tool_calls};
use crate::utils::format_json_spaced;

/// How often to emit a `ping` heartbeat frame while waiting for upstream
/// content, so intermediate proxies/clients do not close an idle SSE
/// connection.
pub const DEFAULT_PING_INTERVAL: std::time::Duration = std::time::Duration::from_secs(15);

/// Encodes a single named SSE frame (`event: <type>\ndata: <json>\n\n`) for
/// the given serializable payload, using spaced JSON formatting (see
/// [`format_json_spaced`]) to match Kiro's expected wire format
/// byte-for-byte.
///
/// # Examples
///
/// ```
/// use lanius_core::api::anthropic_format_sse_event as format_sse_event;
/// use serde_json::json;
///
/// let frame = format_sse_event("ping", &json!({"type": "ping"})).expect("serializable");
/// assert!(frame.starts_with("event: ping\ndata:"));
/// ```
pub fn format_sse_event<T: Serialize>(event_type: &str, data: &T) -> Result<String> {
    let value = serde_json::to_value(data)?;
    Ok(format!(
        "event: {event_type}\ndata: {}\n\n",
        format_json_spaced(&value)
    ))
}

/// Generates a random Anthropic-style message id (`msg_<24 hex chars>`).
///
/// # Examples
///
/// ```
/// use lanius_core::api::anthropic_generate_message_id as generate_message_id;
///
/// let id = generate_message_id();
/// assert!(id.starts_with("msg_"));
/// ```
pub fn generate_message_id() -> String {
    let hex = uuid::Uuid::new_v4().simple().to_string();
    format!("msg_{}", &hex[..24])
}

/// Generates a placeholder signature for a thinking block whose upstream
/// stream ended without a real `signature` (Kiro normally sends one at the
/// end of each native thinking block). Non-verifiable; it only exists so
/// client SDKs that expect the field always find one.
///
/// # Examples
///
/// ```
/// use lanius_core::api::anthropic_generate_thinking_signature as generate_thinking_signature;
///
/// let signature = generate_thinking_signature();
/// assert!(signature.starts_with("sig_"));
/// ```
pub fn generate_thinking_signature() -> String {
    format!("sig_{}", uuid::Uuid::new_v4().simple())
}

/// The pieces of a request needed to estimate its prompt token count
/// before the model responds.
#[derive(Debug, Clone)]
pub struct RequestTokenInput {
    /// The conversation's messages, in whatever raw JSON shape the client
    /// sent them.
    pub messages: Vec<Value>,
    /// Tool definitions offered to the model, if any.
    pub tools: Option<Vec<Value>>,
    /// The system prompt, if any.
    pub system: Option<Value>,
}

impl RequestTokenInput {
    /// Estimates total prompt tokens for this request. `corrected`
    /// enables an additional correction pass in the estimator (see
    /// [`estimate_request_tokens`]) used for some client integrations.
    ///
    /// # Examples
    ///
    /// ```
    /// use lanius_core::api::RequestTokenInput;
    /// use serde_json::json;
    ///
    /// let input = RequestTokenInput {
    ///     messages: vec![json!({"role": "user", "content": "hello"})],
    ///     tools: None,
    ///     system: None,
    /// };
    /// assert!(input.estimate(false) > 0);
    /// ```
    pub fn estimate(&self, corrected: bool) -> usize {
        estimate_request_tokens(
            &self.messages,
            self.tools.as_deref(),
            self.system.as_ref(),
            corrected,
        )
        .total_tokens
    }
}

/// Stateful encoder that turns a sequence of Kiro upstream events into
/// Anthropic Messages API SSE frames.
///
/// Tracks which content block is currently "open" (`thinking_open` /
/// `text_open`) so consecutive events of the same kind are coalesced into
/// a single content block's deltas, while a change in kind (e.g. thinking
/// -> text, or a tool call) closes the previous block and advances
/// `current_index` before opening the next one. Also accumulates the full
/// response text/thinking and cache usage so [`Self::finish`] can compute
/// final token counts, detect truncation, and choose the correct
/// `stop_reason`.
pub struct AnthropicSseFormatter {
    model: String,
    model_cache: ModelInfoCache,
    input_tokens: usize,
    conversation_id: String,
    truncation_store: Option<TruncationStore>,
    started: bool,
    error_emitted: bool,
    current_index: i64,
    thinking_open: Option<i64>,
    text_open: Option<i64>,
    full_content: String,
    alias_text_pending: String,
    full_thinking: String,
    thinking_signature: Option<String>,
    context_usage: Option<f64>,
    cache_usage: Map<String, Value>,
    tool_count: usize,
    tool_name_aliases: ToolNameAliases,
}

impl AnthropicSseFormatter {
    /// Creates a formatter for a single request/response cycle.
    ///
    /// `request` is used only to compute the initial `input_tokens`
    /// reported in `message_start` (before any output has been generated);
    /// `truncation_store` is `None` when truncation recovery is disabled,
    /// in which case truncation records are simply never saved.
    ///
    /// # Examples
    ///
    /// ```
    /// use lanius_core::api::{AnthropicSseFormatter, RequestTokenInput};
    /// use lanius_core::model::ModelInfoCache;
    ///
    /// let request = RequestTokenInput { messages: vec![], tools: None, system: None };
    /// let mut formatter = AnthropicSseFormatter::new(
    ///     "claude-sonnet-4.5",
    ///     ModelInfoCache::default(),
    ///     request,
    ///     "conv-1",
    ///     None,
    /// );
    /// let frames = formatter.start().expect("valid frames");
    /// assert!(frames[0].starts_with("event: message_start"));
    /// ```
    pub fn new(
        model: impl Into<String>,
        model_cache: ModelInfoCache,
        request: RequestTokenInput,
        conversation_id: impl Into<String>,
        truncation_store: Option<TruncationStore>,
    ) -> Self {
        Self {
            model: model.into(),
            model_cache,
            input_tokens: request.estimate(false),
            conversation_id: conversation_id.into(),
            truncation_store,
            started: false,
            error_emitted: false,
            current_index: 0,
            thinking_open: None,
            text_open: None,
            full_content: String::new(),
            alias_text_pending: String::new(),
            full_thinking: String::new(),
            thinking_signature: None,
            context_usage: None,
            cache_usage: Map::new(),
            tool_count: 0,
            tool_name_aliases: ToolNameAliases::default(),
        }
    }

    /// Attaches the per-request tool name alias table (see
    /// [`crate::compat::ToolNameAliases`]) so tool names/text mentioning
    /// aliased tool names are restored to their original form before being
    /// sent to the client.
    ///
    /// # Examples
    ///
    /// ```
    /// use lanius_core::api::{AnthropicSseFormatter, RequestTokenInput};
    /// use lanius_core::compat::ToolNameAliases;
    /// use lanius_core::model::ModelInfoCache;
    ///
    /// let request = RequestTokenInput { messages: vec![], tools: None, system: None };
    /// let formatter = AnthropicSseFormatter::new(
    ///     "claude-sonnet-4.5", ModelInfoCache::default(), request, "conv-1", None,
    /// ).with_tool_name_aliases(ToolNameAliases::default());
    /// ```
    pub fn with_tool_name_aliases(mut self, aliases: ToolNameAliases) -> Self {
        self.tool_name_aliases = aliases;
        self
    }

    /// Emits the `message_start` frame, if not already emitted. Idempotent:
    /// calling this more than once returns an empty frame list after the
    /// first call, so callers do not need to track whether it was already
    /// sent.
    ///
    /// # Examples
    ///
    /// ```
    /// use lanius_core::api::{AnthropicSseFormatter, RequestTokenInput};
    /// use lanius_core::model::ModelInfoCache;
    ///
    /// let request = RequestTokenInput { messages: vec![], tools: None, system: None };
    /// let mut formatter = AnthropicSseFormatter::new(
    ///     "claude-sonnet-4.5", ModelInfoCache::default(), request, "conv-1", None,
    /// );
    /// assert_eq!(formatter.start().unwrap().len(), 1);
    /// // Idempotent: the second call emits nothing.
    /// assert!(formatter.start().unwrap().is_empty());
    /// ```
    pub fn start(&mut self) -> Result<Vec<String>> {
        if self.started {
            return Ok(Vec::new());
        }
        self.started = true;
        Ok(vec![format_sse_event(
            "message_start",
            &json!({
                "type": "message_start",
                "message": {
                    "id": generate_message_id(),
                    "type": "message",
                    "role": "assistant",
                    "content": [],
                    "model": self.model,
                    "stop_reason": Value::Null,
                    "stop_sequence": Value::Null,
                    "usage": {"input_tokens": self.input_tokens, "output_tokens": 0}
                }
            }),
        )?])
    }

    /// Builds a single `ping` heartbeat frame.
    ///
    /// # Examples
    ///
    /// ```
    /// use lanius_core::api::{AnthropicSseFormatter, RequestTokenInput};
    /// use lanius_core::model::ModelInfoCache;
    ///
    /// let request = RequestTokenInput { messages: vec![], tools: None, system: None };
    /// let formatter = AnthropicSseFormatter::new(
    ///     "claude-sonnet-4.5", ModelInfoCache::default(), request, "conv-1", None,
    /// );
    /// let frame = formatter.ping().expect("valid frame");
    /// assert!(frame.starts_with("event: ping"));
    /// ```
    pub fn ping(&self) -> Result<String> {
        format_sse_event("ping", &json!({"type": "ping"}))
    }

    /// Feeds one parsed upstream Kiro event into the state machine,
    /// returning the SSE frames (zero or more) it produces.
    ///
    /// This is the core of the content-block state machine:
    /// - `Content`: appends to `full_content`; closes any open thinking
    ///   block (text and thinking never interleave within one turn once
    ///   text starts); opens a text block if one is not already open at
    ///   `current_index`; then emits a `content_block_delta` with the
    ///   alias-restored text fragment (buffered via
    ///   `alias_text_pending` so a tool name alias split across two Kiro
    ///   chunks is not leaked to the client mid-token).
    /// - `Thinking`: native thinking text opens (after closing any open text
    ///   block) or extends a `thinking` block via `thinking_delta` frames; a
    ///   signature is remembered and emitted as `signature_delta` when the
    ///   thinking block closes.
    /// - `ToolUse`: closes any open text/thinking block (a tool call always
    ///   starts a new content block) and delegates to [`Self::emit_tool`].
    /// - `ContextUsage`/`Usage`: recorded for use by [`Self::finish`]; they
    ///   never emit frames on their own.
    /// - `Error`: delegates to [`Self::error`], terminating the stream.
    ///
    /// # Examples
    ///
    /// ```
    /// use lanius_core::api::{AnthropicSseFormatter, RequestTokenInput};
    /// use lanius_core::model::ModelInfoCache;
    /// use lanius_core::upstream::KiroEvent;
    ///
    /// let request = RequestTokenInput { messages: vec![], tools: None, system: None };
    /// let mut formatter = AnthropicSseFormatter::new(
    ///     "claude-sonnet-4.5", ModelInfoCache::default(), request, "conv-1", None,
    /// );
    /// formatter.start().expect("valid frames");
    /// let frames = formatter.push(KiroEvent::content("hello")).expect("valid frames");
    /// assert!(!frames.is_empty());
    /// ```
    pub fn push(&mut self, event: KiroEvent) -> Result<Vec<String>> {
        let mut frames = Vec::new();
        match event.event_type {
            KiroEventType::Content => {
                let content = event.content.unwrap_or_default();
                self.full_content.push_str(&content);
                self.close_thinking(&mut frames)?;
                if self.text_open.is_none() {
                    let index = self.current_index;
                    self.text_open = Some(index);
                    frames.push(format_sse_event(
                        "content_block_start",
                        &json!({"type":"content_block_start","index":index,"content_block":{"type":"text","text":""}}),
                    )?);
                }
                let client_content = self.tool_name_aliases.restore_text_fragment(
                    &mut self.alias_text_pending,
                    &content,
                    false,
                );
                if !client_content.is_empty() {
                    let index = self.text_open.unwrap_or_default();
                    frames.push(format_sse_event(
                        "content_block_delta",
                        &json!({"type":"content_block_delta","index":index,"delta":{"type":"text_delta","text":client_content}}),
                    )?);
                }
            }
            KiroEventType::Thinking => {
                let thinking = event.thinking_content.unwrap_or_default();
                if !thinking.is_empty() {
                    self.full_thinking.push_str(&thinking);
                    if self.thinking_open.is_none() {
                        self.close_text(&mut frames)?;
                        let index = self.current_index;
                        self.thinking_open = Some(index);
                        frames.push(format_sse_event(
                            "content_block_start",
                            &json!({"type":"content_block_start","index":index,"content_block":{"type":"thinking","thinking":"","signature":""}}),
                        )?);
                    }
                    let index = self.thinking_open.unwrap_or_default();
                    frames.push(format_sse_event(
                        "content_block_delta",
                        &json!({"type":"content_block_delta","index":index,"delta":{"type":"thinking_delta","thinking":thinking}}),
                    )?);
                }
                if event.thinking_signature.is_some() {
                    self.thinking_signature = event.thinking_signature;
                }
            }
            KiroEventType::ToolUse => {
                self.close_open_blocks(&mut frames)?;
                if let Some(tool) = event.tool_use {
                    self.emit_tool(&mut frames, tool)?;
                }
            }
            KiroEventType::ContextUsage => {
                if let Some(percentage) = event.context_usage_percentage {
                    self.context_usage = Some(percentage);
                }
            }
            KiroEventType::Usage => {
                if let Some(usage) = event.usage {
                    extract_cache_usage(&usage, &mut self.cache_usage);
                }
            }
            KiroEventType::Error => {
                return self.error(event.content.as_deref().unwrap_or("upstream stream error"));
            }
        }
        Ok(frames)
    }

    /// Finalizes the stream: flushes any buffered alias text, closes any
    /// still-open content block, and parses `[Called tool with args:
    /// ...]`-style bracket tool calls out of the accumulated text (some
    /// upstream configurations emit tool calls this way instead of as
    /// dedicated `ToolUse` events — see [`parse_bracket_tool_calls`]).
    ///
    /// Computes final output token counts (correcting `input_tokens` from
    /// the upstream's reported context-usage percentage when available),
    /// determines the `stop_reason` (`max_tokens` if content was
    /// truncated, `tool_use` if any tool was called, else `end_turn`),
    /// saves a truncation record if truncation was detected, and emits the
    /// terminal `message_delta` + `message_stop` frames. A no-op if
    /// [`Self::error`] was already called for this stream.
    ///
    /// # Examples
    ///
    /// ```
    /// use lanius_core::api::{AnthropicSseFormatter, RequestTokenInput};
    /// use lanius_core::model::ModelInfoCache;
    /// use lanius_core::upstream::KiroEvent;
    ///
    /// let request = RequestTokenInput { messages: vec![], tools: None, system: None };
    /// let mut formatter = AnthropicSseFormatter::new(
    ///     "claude-sonnet-4.5", ModelInfoCache::default(), request, "conv-1", None,
    /// );
    /// formatter.start().expect("valid frames");
    /// formatter.push(KiroEvent::content("hello")).expect("valid frames");
    /// let frames = formatter.finish().expect("valid frames");
    /// assert!(frames.last().unwrap().contains("message_stop"));
    /// ```
    pub fn finish(&mut self) -> Result<Vec<String>> {
        let mut frames = Vec::new();
        if self.error_emitted {
            return Ok(frames);
        }
        self.flush_alias_text(&mut frames)?;
        self.close_open_blocks(&mut frames)?;
        for tool in parse_bracket_tool_calls(&self.full_content) {
            self.emit_tool(&mut frames, tool)?;
        }
        let output_tokens = count_tokens(
            &format!("{}{}", self.full_content, self.full_thinking),
            false,
        );
        let context_tokens = calculate_tokens_from_context_usage(
            self.context_usage,
            output_tokens,
            &self.model_cache,
            &self.model,
        );
        if context_tokens.prompt_source != "unknown" {
            self.input_tokens = context_tokens.prompt_tokens;
        }
        let content_truncated = is_content_truncated(
            self.context_usage.is_some(),
            &self.full_content,
            self.tool_count > 0,
        );
        if content_truncated {
            if let Some(store) = &self.truncation_store {
                store.save_content_truncation(&self.conversation_id, &self.full_content);
            }
        }
        let stop_reason = if content_truncated {
            "max_tokens"
        } else if self.tool_count > 0 {
            "tool_use"
        } else {
            "end_turn"
        };
        let mut usage = Map::new();
        usage.insert("output_tokens".to_string(), json!(output_tokens));
        usage.extend(self.cache_usage.clone());
        frames.push(format_sse_event(
            "message_delta",
            &json!({"type":"message_delta","delta":{"stop_reason":stop_reason,"stop_sequence":Value::Null},"usage":usage}),
        )?);
        frames.push(format_sse_event(
            "message_stop",
            &json!({"type":"message_stop"}),
        )?);
        Ok(frames)
    }

    /// Emits a terminal `error` frame and marks the stream as finished.
    /// Idempotent: a second call returns an empty frame list. Truncates
    /// `message` via [`safe_error_message`] to avoid an unbounded error
    /// payload reaching the client.
    ///
    /// # Examples
    ///
    /// ```
    /// use lanius_core::api::{AnthropicSseFormatter, RequestTokenInput};
    /// use lanius_core::model::ModelInfoCache;
    ///
    /// let request = RequestTokenInput { messages: vec![], tools: None, system: None };
    /// let mut formatter = AnthropicSseFormatter::new(
    ///     "claude-sonnet-4.5", ModelInfoCache::default(), request, "conv-1", None,
    /// );
    /// let frames = formatter.error("upstream failed").expect("valid frames");
    /// assert!(frames[0].contains("api_error"));
    /// // Idempotent: a second call emits nothing.
    /// assert!(formatter.error("again").unwrap().is_empty());
    /// ```
    pub fn error(&mut self, message: &str) -> Result<Vec<String>> {
        if self.error_emitted {
            return Ok(Vec::new());
        }
        self.error_emitted = true;
        Ok(vec![format_sse_event(
            "error",
            &json!({"type":"error","error":{"type":"api_error","message":safe_error_message(message)}}),
        )?])
    }

    /// Emits any text still buffered in `alias_text_pending` (a possible
    /// partial tool-name alias fragment held back in case more bytes were
    /// about to complete the alias) as a final `content_block_delta`,
    /// called once at [`Self::finish`] since no further content is coming.
    fn flush_alias_text(&mut self, frames: &mut Vec<String>) -> Result<()> {
        let text =
            self.tool_name_aliases
                .restore_text_fragment(&mut self.alias_text_pending, "", true);
        if text.is_empty() {
            return Ok(());
        }
        if self.text_open.is_none() {
            let index = self.current_index;
            self.text_open = Some(index);
            frames.push(format_sse_event(
                "content_block_start",
                &json!({"type":"content_block_start","index":index,"content_block":{"type":"text","text":""}}),
            )?);
        }
        let index = self.text_open.unwrap_or_default();
        frames.push(format_sse_event(
            "content_block_delta",
            &json!({"type":"content_block_delta","index":index,"delta":{"type":"text_delta","text":text}}),
        )?);
        Ok(())
    }

    /// Closes the currently open thinking block, if any: emits its
    /// signature (the upstream one, or a placeholder if none arrived) as a
    /// `signature_delta`, then `content_block_stop`, and advances
    /// `current_index`.
    fn close_thinking(&mut self, frames: &mut Vec<String>) -> Result<()> {
        if let Some(index) = self.thinking_open.take() {
            let signature = self
                .thinking_signature
                .take()
                .unwrap_or_else(generate_thinking_signature);
            frames.push(format_sse_event(
                "content_block_delta",
                &json!({"type":"content_block_delta","index":index,"delta":{"type":"signature_delta","signature":signature}}),
            )?);
            frames.push(format_sse_event(
                "content_block_stop",
                &json!({"type":"content_block_stop","index":index}),
            )?);
            self.current_index += 1;
        }
        Ok(())
    }

    /// Closes whichever content block is currently open (thinking and/or
    /// text), emitting `content_block_stop` for each and advancing
    /// `current_index` past them. Called before starting a tool-use block
    /// and at the end of the stream.
    fn close_open_blocks(&mut self, frames: &mut Vec<String>) -> Result<()> {
        self.close_thinking(frames)?;
        self.close_text(frames)
    }

    /// Closes the currently open text block, if any, emitting
    /// `content_block_stop` and advancing `current_index`.
    fn close_text(&mut self, frames: &mut Vec<String>) -> Result<()> {
        if let Some(index) = self.text_open.take() {
            frames.push(format_sse_event(
                "content_block_stop",
                &json!({"type":"content_block_stop","index":index}),
            )?);
            self.current_index += 1;
        }
        Ok(())
    }

    /// Emits the three-frame sequence for a single tool call
    /// (`content_block_start` -> `content_block_delta` with the full
    /// arguments as one `input_json_delta` -> `content_block_stop`),
    /// restoring the tool's original (pre-alias) name for the client via
    /// `tool_name_aliases.original_for`. Unlike text, tool arguments are
    /// not streamed incrementally — Kiro delivers them as a complete JSON
    /// blob, so a single delta frame carries the whole `partial_json`.
    /// Also records a truncation entry if the parsed [`ToolCall`] reports
    /// one, and increments `tool_count`/`current_index`.
    fn emit_tool(&mut self, frames: &mut Vec<String>, tool: ToolCall) -> Result<()> {
        let index = self.current_index;
        let id = tool
            .id
            .filter(|id| !id.is_empty())
            .unwrap_or_else(generate_tool_id);
        let input = parse_tool_input(&tool.arguments);
        frames.push(format_sse_event(
            "content_block_start",
            &json!({"type":"content_block_start","index":index,"content_block":{"type":"tool_use","id":id,"name":self.tool_name_aliases.original_for(&tool.name),"input":{}}}),
        )?);
        frames.push(format_sse_event(
            "content_block_delta",
            &json!({"type":"content_block_delta","index":index,"delta":{"type":"input_json_delta","partial_json":format_json_spaced(&input)}}),
        )?);
        frames.push(format_sse_event(
            "content_block_stop",
            &json!({"type":"content_block_stop","index":index}),
        )?);
        if let (Some(store), Some(info)) = (&self.truncation_store, tool.truncation) {
            let value = json!({"is_truncated":info.is_truncated,"reason":info.reason,"size_bytes":info.size_bytes});
            store.save_tool_truncation(&self.conversation_id, id, tool.name, value);
        }
        self.tool_count += 1;
        self.current_index += 1;
        Ok(())
    }
}

/// Builds the full non-streaming `/v1/messages` JSON response from a
/// collected [`StreamResult`] (used when `request.stream` is `false`).
///
/// Restores original tool names from `aliases` in both the response text
/// and each tool call's `name`, computes final token counts (preferring
/// the upstream's context-usage-derived prompt token count when
/// available), assembles the `content` array (native thinking block with
/// its upstream signature when present, then text, then tool-use blocks),
/// and picks `stop_reason` the same way
/// [`AnthropicSseFormatter::finish`] does for streaming responses.
///
/// # Examples
///
/// ```
/// use lanius_core::api::{response_from_stream_result, RequestTokenInput};
/// use lanius_core::compat::ToolNameAliases;
/// use lanius_core::model::ModelInfoCache;
/// use lanius_core::upstream::StreamResult;
///
/// let result = StreamResult { content: "hello".into(), ..StreamResult::default() };
/// let request = RequestTokenInput { messages: vec![], tools: None, system: None };
/// let response = response_from_stream_result(
///     result, "claude-sonnet-4.5".into(), &ModelInfoCache::default(),
///     &request, &ToolNameAliases::default(),
/// );
/// assert_eq!(response["content"][0]["text"], "hello");
/// ```
pub fn response_from_stream_result(
    mut result: StreamResult,
    model: String,
    model_cache: &ModelInfoCache,
    request: &RequestTokenInput,
    aliases: &ToolNameAliases,
) -> Value {
    result.content = aliases.restore_text(&result.content);
    for tool in &mut result.tool_calls {
        tool.name = aliases.original_for(&tool.name);
    }
    let mut input_tokens = request.estimate(false);
    let output_tokens = count_tokens(
        &format!("{}{}", result.content, result.thinking_content),
        false,
    );
    let context = calculate_tokens_from_context_usage(
        result.context_usage_percentage,
        output_tokens,
        model_cache,
        &model,
    );
    if context.prompt_source != "unknown" {
        input_tokens = context.prompt_tokens;
    }
    let mut content = Vec::new();
    if !result.thinking_content.is_empty() {
        let signature = result
            .thinking_signature
            .clone()
            .unwrap_or_else(generate_thinking_signature);
        content.push(
            json!({"type":"thinking","thinking":result.thinking_content,"signature":signature}),
        );
    }
    let text = result.content.clone();
    if !text.is_empty() {
        content.push(json!({"type":"text","text":text}));
    }
    for tool in &result.tool_calls {
        let id = tool
            .id
            .clone()
            .filter(|id| !id.is_empty())
            .unwrap_or_else(generate_tool_id);
        content.push(json!({"type":"tool_use","id":id,"name":tool.name,"input":parse_tool_input(&tool.arguments)}));
    }
    let truncated = is_content_truncated(
        result.context_usage_percentage.is_some(),
        &result.content,
        !result.tool_calls.is_empty(),
    );
    let reason = if truncated {
        "max_tokens"
    } else if !result.tool_calls.is_empty() {
        "tool_use"
    } else {
        "end_turn"
    };
    let mut usage = Map::new();
    usage.insert("input_tokens".into(), json!(input_tokens));
    usage.insert("output_tokens".into(), json!(output_tokens));
    if let Some(upstream) = result.usage {
        extract_cache_usage(&upstream, &mut usage);
    }
    json!({"id":generate_message_id(),"type":"message","role":"assistant","content":content,"model":model,"stop_reason":reason,"stop_sequence":Value::Null,"usage":usage})
}

/// Parses a tool's raw JSON arguments string, falling back to an empty
/// object if the string is not valid JSON or is not a JSON object (e.g.
/// truncated/malformed upstream output).
fn parse_tool_input(arguments: &str) -> Value {
    serde_json::from_str::<Value>(arguments)
        .ok()
        .filter(Value::is_object)
        .unwrap_or_else(|| Value::Object(Map::new()))
}

/// Generates a random Anthropic-style tool-use id (`toolu_<24 hex
/// chars>`), used as a fallback when the upstream did not supply one.
fn generate_tool_id() -> String {
    let hex = uuid::Uuid::new_v4().simple().to_string();
    format!("toolu_{}", &hex[..24])
}

/// Copies prompt-cache token counts from an upstream usage object into the
/// Anthropic usage map, accepting both snake_case and camelCase upstream
/// key spellings.
fn extract_cache_usage(usage: &Value, target: &mut Map<String, Value>) {
    let Some(object) = usage.as_object() else {
        return;
    };
    for (source, destination) in [
        ("cache_read_input_tokens", "cache_read_input_tokens"),
        ("cacheReadInputTokens", "cache_read_input_tokens"),
        ("cache_creation_input_tokens", "cache_creation_input_tokens"),
        ("cacheCreationInputTokens", "cache_creation_input_tokens"),
    ] {
        if let Some(value) = object.get(source).and_then(Value::as_i64) {
            target.insert(destination.to_string(), json!(value));
        }
    }
}

/// Truncates an error message to 500 characters before sending it to the
/// client, bounding the size of the `error` SSE frame.
fn safe_error_message(message: &str) -> String {
    message.chars().take(500).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::upstream::parser::ToolCall;

    fn formatter() -> AnthropicSseFormatter {
        AnthropicSseFormatter::new(
            "claude",
            ModelInfoCache::default(),
            RequestTokenInput {
                messages: vec![json!({"role":"user","content":"hi"})],
                tools: None,
                system: None,
            },
            "conversation",
            None,
        )
    }
    fn event_names(frames: &[String]) -> Vec<&str> {
        frames
            .iter()
            .filter_map(|frame| {
                frame
                    .strip_prefix("event: ")
                    .and_then(|rest| rest.split('\n').next())
            })
            .collect()
    }

    #[test]
    fn complete_thinking_text_tool_sequence_is_strict_and_indexed() {
        let mut formatter = formatter();
        let mut frames = formatter.start().unwrap_or_default();
        frames.extend(
            formatter
                .push(KiroEvent::thinking(Some("reason".into()), None))
                .unwrap_or_default(),
        );
        frames.extend(
            formatter
                .push(KiroEvent::thinking(None, Some("sig_upstream".into())))
                .unwrap_or_default(),
        );
        frames.extend(
            formatter
                .push(KiroEvent::content("answer"))
                .unwrap_or_default(),
        );
        frames.extend(
            formatter
                .push(KiroEvent::tool_use(ToolCall {
                    id: Some("toolu_1".into()),
                    name: "weather".into(),
                    arguments: r#"{"city": "北京"}"#.into(),
                    truncation: None,
                }))
                .unwrap_or_default(),
        );
        frames.extend(
            formatter
                .push(KiroEvent::context_usage(Some(1.0)))
                .unwrap_or_default(),
        );
        frames.extend(formatter.finish().unwrap_or_default());
        assert_eq!(
            event_names(&frames),
            vec![
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_delta",
                "content_block_stop",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop"
            ]
        );
        assert!(frames[1].contains("\"index\": 0"));
        assert!(frames[2].contains("thinking_delta"));
        assert!(frames[3].contains("signature_delta") && frames[3].contains("sig_upstream"));
        assert!(frames[5].contains("\"index\": 1"));
        assert!(frames[8].contains("\"index\": 2"));
        assert!(frames[9].contains("input_json_delta"));
        assert!(frames[9].contains(r#"{\"city\": \"北京\"}"#));
    }

    #[test]
    fn start_and_error_are_each_emitted_once() {
        let mut formatter = formatter();
        let mut frames = formatter.start().unwrap_or_default();
        frames.extend(formatter.start().unwrap_or_default());
        frames.extend(formatter.error("broken").unwrap_or_default());
        frames.extend(formatter.error("again").unwrap_or_default());
        assert_eq!(event_names(&frames), vec!["message_start", "error"]);
    }

    #[test]
    fn ping_is_a_real_protocol_frame() {
        let formatter = formatter();
        assert_eq!(
            formatter.ping().unwrap_or_default(),
            "event: ping\ndata: {\"type\": \"ping\"}\n\n"
        );
    }

    #[test]
    fn tool_arguments_are_not_fragmented_and_invalid_json_is_object() {
        let mut formatter = formatter();
        let mut frames = formatter.start().unwrap_or_default();
        frames.extend(
            formatter
                .push(KiroEvent::tool_use(ToolCall {
                    id: Some("t".into()),
                    name: "f".into(),
                    arguments: "invalid".into(),
                    truncation: None,
                }))
                .unwrap_or_default(),
        );
        assert_eq!(
            event_names(&frames),
            vec![
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_stop"
            ]
        );
        assert!(frames[2].contains("\"partial_json\": \"{}\""));
    }

    #[test]
    fn usage_and_truncation_stop_reason_follow_source_priority() {
        let mut formatter = formatter();
        let mut frames = formatter.start().unwrap_or_default();
        frames.extend(
            formatter
                .push(KiroEvent::content("cut off"))
                .unwrap_or_default(),
        );
        frames.extend(formatter.finish().unwrap_or_default());
        assert!(frames.iter().any(|frame| frame.contains("max_tokens")));
        let response = response_from_stream_result(
            StreamResult {
                content: "ok".into(),
                ..StreamResult::default()
            },
            "claude".into(),
            &ModelInfoCache::default(),
            &RequestTokenInput {
                messages: vec![],
                tools: None,
                system: None,
            },
            &ToolNameAliases::default(),
        );
        assert_eq!(response["stop_reason"], "max_tokens");
    }
    #[test]
    fn request_scoped_aliases_round_trip_anthropic_conversion_sse_and_collection() {
        let original = "mcp__plugin_everything_claude_code_github__create_pull_request_review";
        let request: crate::api::anthropic::models::AnthropicMessagesRequest = serde_json::from_value(json!({
            "model":"claude", "max_tokens":1, "messages":[{"role":"user","content":"use the tool"}],
            "tools":[{"name":original,"input_schema":{"type":"object"}}]
        })).unwrap_or_else(|error| panic!("fixture request must deserialize: {error}"));
        let mut aliases = ToolNameAliases::default();
        let payload = crate::convert::anthropic::anthropic_to_kiro(
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
        assert!(payload.to_string().contains(&alias));

        let mut formatter = AnthropicSseFormatter::new(
            "claude",
            ModelInfoCache::default(),
            RequestTokenInput {
                messages: vec![],
                tools: None,
                system: None,
            },
            "conversation",
            None,
        )
        .with_tool_name_aliases(aliases.clone());
        let split_at = alias.chars().count() / 2;
        let alias_prefix: String = alias.chars().take(split_at).collect();
        let alias_suffix: String = alias.chars().skip(split_at).collect();
        let mut frames = formatter
            .start()
            .unwrap_or_else(|error| panic!("start must encode: {error}"));
        frames.extend(
            formatter
                .push(KiroEvent::content(format!("[Called {alias_prefix}")))
                .unwrap_or_else(|error| panic!("first content must encode: {error}")),
        );
        frames.extend(
            formatter
                .push(KiroEvent::content(format!(
                    "{alias_suffix} with args: {{}}]"
                )))
                .unwrap_or_else(|error| panic!("second content must encode: {error}")),
        );
        frames.extend(
            formatter
                .push(KiroEvent::tool_use(ToolCall {
                    id: Some("call".into()),
                    name: alias.clone(),
                    arguments: "{}".into(),
                    truncation: None,
                }))
                .unwrap_or_else(|error| panic!("tool must encode: {error}")),
        );
        frames.extend(
            formatter
                .finish()
                .unwrap_or_else(|error| panic!("finish must encode: {error}")),
        );
        let wire = frames.join("");
        assert!(wire.contains(original));
        assert!(!wire.contains(&format!("[Called {alias} with args:")));

        let collected = response_from_stream_result(
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
            "claude".into(),
            &ModelInfoCache::default(),
            &RequestTokenInput {
                messages: vec![],
                tools: None,
                system: None,
            },
            &aliases,
        );
        assert_eq!(collected["content"][1]["name"], original);
        assert!(
            collected["content"][0]["text"]
                .as_str()
                .is_some_and(|text| text.contains(original))
        );
    }
}

#[cfg(test)]
mod native_thinking_tests {
    use super::*;

    fn request() -> RequestTokenInput {
        RequestTokenInput {
            messages: vec![],
            tools: None,
            system: None,
        }
    }

    #[test]
    fn thinking_after_text_closes_text_and_missing_signature_gets_placeholder() {
        let mut formatter = AnthropicSseFormatter::new(
            "claude",
            ModelInfoCache::default(),
            request(),
            "conversation",
            None,
        );
        let mut frames = formatter.push(KiroEvent::content("a")).unwrap_or_default();
        frames.extend(
            formatter
                .push(KiroEvent::thinking(Some("t".into()), None))
                .unwrap_or_default(),
        );
        frames.extend(formatter.finish().unwrap_or_default());
        let wire = frames.join("");
        assert!(wire.contains(r#""type": "text""#));
        assert!(frames[2].contains("content_block_stop") && frames[2].contains("\"index\": 0"));
        assert!(frames[3].contains(r#""type": "thinking""#) && frames[3].contains("\"index\": 1"));
        assert!(wire.contains(r#""signature": "sig_"#));
    }

    #[test]
    fn non_stream_response_uses_upstream_signature() {
        let response = response_from_stream_result(
            StreamResult {
                content: "answer".into(),
                thinking_content: "private reasoning".into(),
                thinking_signature: Some("sig_upstream".into()),
                context_usage_percentage: Some(1.0),
                ..StreamResult::default()
            },
            "claude".into(),
            &ModelInfoCache::default(),
            &request(),
            &ToolNameAliases::default(),
        );
        assert_eq!(
            response["content"],
            json!([
                {"type": "thinking", "thinking": "private reasoning", "signature": "sig_upstream"},
                {"type": "text", "text": "answer"}
            ])
        );
    }
}
