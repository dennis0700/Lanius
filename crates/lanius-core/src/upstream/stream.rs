//! Wraps [`AwsEventStreamParser`]
//! to produce the crate-wide stream of provider-agnostic [`KiroEvent`]s.
//!
//! This is the layer the API route handlers in [`crate::api`] actually
//! consume: [`parse_kiro_stream`] (and its config-driven wrapper
//! [`parse_kiro_stream_with_config`]) turns the raw upstream byte stream
//! into an `async` [`Stream`] of [`KiroEvent`]s, applying the configured
//! first-token/streaming-read timeouts. Native reasoning
//! (`reasoningContentEvent`) arrives as its own upstream event and is
//! surfaced as [`KiroEventType::Thinking`]. [`collect_stream_to_result`] (and its
//! wrapper [`collect_stream_to_result_with_config`]) drains that event
//! stream into a single [`StreamResult`] for callers that need the whole
//! response at once (e.g. building a non-streaming API response), also
//! merging in any bracket-style tool calls found in the final concatenated
//! content.

use std::time::Duration;

use async_stream::try_stream;
use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use serde_json::Value;

use super::parser::{
    AwsEventStreamParser, ParserEvent, ToolCall, deduplicate_tool_calls, parse_bracket_tool_calls,
};
use crate::config::Config;
use crate::error::{GatewayError, Result, classify_network_error};

/// Discriminant for [`KiroEvent`], identifying which of its optional fields
/// is populated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KiroEventType {
    /// Assistant text content is populated.
    Content,
    /// Native thinking content (and possibly its closing signature) is
    /// populated.
    Thinking,
    /// A completed tool call is populated.
    ToolUse,
    /// Usage/billing information is populated.
    Usage,
    /// Context-window usage percentage is populated.
    ContextUsage,
    /// The stream ended with an error.
    Error,
}

/// A single provider-agnostic event produced by [`parse_kiro_stream`].
/// Which fields are populated depends on `event_type`; unrelated fields are
/// always `None`/default. Constructed via the associated helper functions
/// ([`KiroEvent::content`], [`KiroEvent::thinking`], etc.) rather than
/// built directly, since each variant only makes sense with a specific
/// combination of fields set.
#[derive(Debug, Clone, PartialEq)]
pub struct KiroEvent {
    /// Which of the fields below is populated for this event.
    pub event_type: KiroEventType,
    /// Assistant text content, for [`KiroEventType::Content`] events.
    pub content: Option<String>,
    /// Native thinking text, for [`KiroEventType::Thinking`] events.
    pub thinking_content: Option<String>,
    /// Opaque signature closing a native thinking block, when Kiro sent one.
    pub thinking_signature: Option<String>,
    /// The completed tool call, for [`KiroEventType::ToolUse`] events.
    pub tool_use: Option<super::parser::ToolCall>,
    /// Raw usage/billing JSON, for [`KiroEventType::Usage`] events.
    pub usage: Option<serde_json::Value>,
    /// Context-window usage percentage, for [`KiroEventType::ContextUsage`]
    /// events.
    pub context_usage_percentage: Option<f64>,
}

impl KiroEvent {
    /// Builds a [`KiroEventType::Content`] event carrying `content`.
    ///
    /// # Examples
    ///
    /// ```
    /// use lanius_core::upstream::{KiroEvent, KiroEventType};
    ///
    /// let event = KiroEvent::content("hello");
    /// assert_eq!(event.event_type, KiroEventType::Content);
    /// assert_eq!(event.content.as_deref(), Some("hello"));
    /// ```
    pub fn content(content: impl Into<String>) -> Self {
        Self {
            event_type: KiroEventType::Content,
            content: Some(content.into()),
            ..Self::empty(KiroEventType::Content)
        }
    }

    /// Builds a [`KiroEventType::Thinking`] event from a native reasoning
    /// fragment: summarized thinking text, the block's closing signature, or
    /// both.
    ///
    /// # Examples
    ///
    /// ```
    /// use lanius_core::upstream::{KiroEvent, KiroEventType};
    ///
    /// let event = KiroEvent::thinking(Some("reasoning".into()), None);
    /// assert_eq!(event.event_type, KiroEventType::Thinking);
    /// assert_eq!(event.thinking_content.as_deref(), Some("reasoning"));
    /// ```
    pub fn thinking(content: Option<String>, signature: Option<String>) -> Self {
        Self {
            event_type: KiroEventType::Thinking,
            thinking_content: content,
            thinking_signature: signature,
            ..Self::empty(KiroEventType::Thinking)
        }
    }

    /// Builds a [`KiroEventType::ToolUse`] event carrying one finalized
    /// tool call.
    ///
    /// # Examples
    ///
    /// ```
    /// use lanius_core::upstream::{KiroEvent, KiroEventType, ToolCall};
    ///
    /// let call = ToolCall { id: Some("1".into()), name: "read".into(), arguments: "{}".into(), truncation: None };
    /// let event = KiroEvent::tool_use(call);
    /// assert_eq!(event.event_type, KiroEventType::ToolUse);
    /// ```
    pub fn tool_use(tool_use: ToolCall) -> Self {
        Self {
            event_type: KiroEventType::ToolUse,
            tool_use: Some(tool_use),
            ..Self::empty(KiroEventType::ToolUse)
        }
    }

    /// Builds a [`KiroEventType::Usage`] event carrying Kiro's raw usage
    /// JSON.
    ///
    /// # Examples
    ///
    /// ```
    /// use lanius_core::upstream::{KiroEvent, KiroEventType};
    /// use serde_json::json;
    ///
    /// let event = KiroEvent::usage(json!({"inputTokens": 10}));
    /// assert_eq!(event.event_type, KiroEventType::Usage);
    /// ```
    pub fn usage(usage: Value) -> Self {
        Self {
            event_type: KiroEventType::Usage,
            usage: Some(usage),
            ..Self::empty(KiroEventType::Usage)
        }
    }

    /// Builds a [`KiroEventType::ContextUsage`] event carrying the
    /// percentage of context window consumed, if Kiro reported one.
    ///
    /// # Examples
    ///
    /// ```
    /// use lanius_core::upstream::{KiroEvent, KiroEventType};
    ///
    /// let event = KiroEvent::context_usage(Some(42.0));
    /// assert_eq!(event.event_type, KiroEventType::ContextUsage);
    /// assert_eq!(event.context_usage_percentage, Some(42.0));
    /// ```
    pub fn context_usage(percentage: Option<f64>) -> Self {
        Self {
            event_type: KiroEventType::ContextUsage,
            context_usage_percentage: percentage,
            ..Self::empty(KiroEventType::ContextUsage)
        }
    }

    /// Builds a [`KiroEventType::Error`] event carrying an error message.
    ///
    /// # Examples
    ///
    /// ```
    /// use lanius_core::upstream::{KiroEvent, KiroEventType};
    ///
    /// let event = KiroEvent::error("upstream failed");
    /// assert_eq!(event.event_type, KiroEventType::Error);
    /// assert_eq!(event.content.as_deref(), Some("upstream failed"));
    /// ```
    pub fn error(content: impl Into<String>) -> Self {
        Self {
            event_type: KiroEventType::Error,
            content: Some(content.into()),
            ..Self::empty(KiroEventType::Error)
        }
    }

    fn empty(event_type: KiroEventType) -> Self {
        Self {
            event_type,
            content: None,
            thinking_content: None,
            thinking_signature: None,
            tool_use: None,
            usage: None,
            context_usage_percentage: None,
        }
    }
}

/// Aggregate result of fully draining a [`KiroEvent`] stream, as produced by
/// [`collect_stream_to_result`]: concatenated regular content, concatenated
/// native thinking content (plus its signature, if sent), all tool calls
/// (structured plus any bracket-style ones found in the final text), and the
/// last-seen usage/context-usage values.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct StreamResult {
    /// Accumulated assistant text content.
    pub content: String,
    /// Accumulated native thinking text.
    pub thinking_content: String,
    /// Opaque signature closing the thinking block, when Kiro sent one.
    pub thinking_signature: Option<String>,
    /// All tool calls extracted from the stream (structured plus any
    /// bracket-style ones found in the final text), deduplicated.
    pub tool_calls: Vec<ToolCall>,
    /// Last-seen raw usage/billing JSON, if the upstream reported any.
    pub usage: Option<serde_json::Value>,
    /// Last-seen context-window usage percentage, if the upstream reported
    /// one.
    pub context_usage_percentage: Option<f64>,
}

impl StreamResult {
    /// Creates an empty result with no content, tool calls, or usage.
    ///
    /// # Examples
    ///
    /// ```
    /// use lanius_core::upstream::StreamResult;
    ///
    /// let result = StreamResult::new();
    /// assert!(result.content.is_empty());
    /// assert!(result.tool_calls.is_empty());
    /// ```
    pub fn new() -> Self {
        Self::default()
    }
}

/// Turns a raw byte stream from Kiro into a stream of [`KiroEvent`]s.
///
/// Waiting for the very first byte is bounded by `first_token_timeout`
/// (failing with [`GatewayError::FirstTokenTimeout`] if exceeded); every
/// subsequent read is bounded by `streaming_read_timeout` (failing with
/// [`GatewayError::StreamReadTimeout`]) — the timeouts are deliberately
/// asymmetric because waiting for Kiro to start responding at all is a
/// different failure mode than a stall partway through.
///
/// Tool-call events are always emitted last, since Kiro's protocol only
/// finalizes them once the whole response — or at least the whole tool
/// call — has arrived.
///
/// # Examples
///
/// ```
/// use lanius_core::upstream::parse_kiro_stream;
/// use futures_util::StreamExt;
/// use std::time::Duration;
///
/// # async fn example() {
/// let chunks = vec![Ok(bytes::Bytes::from(r#"{"content":"hi"}"#))];
/// let stream = futures_util::stream::iter(chunks);
/// let events = parse_kiro_stream(stream, Duration::from_secs(5), Duration::from_secs(5));
/// let events: Vec<_> = events.collect().await;
/// assert!(!events.is_empty());
/// # }
/// ```
pub fn parse_kiro_stream<S>(
    mut byte_stream: S,
    first_token_timeout: Duration,
    streaming_read_timeout: Duration,
) -> impl Stream<Item = Result<KiroEvent>>
where
    S: Stream<Item = std::result::Result<Bytes, reqwest::Error>> + Send + Unpin + 'static,
{
    try_stream! {
        let mut parser = AwsEventStreamParser::new();

        let first = tokio::time::timeout(first_token_timeout, byte_stream.next())
            .await
            .map_err(|_| GatewayError::FirstTokenTimeout(first_token_timeout))?;
        let Some(first) = first else { return; };
        let first = first.map_err(|error| network_error(&error))?;
        for event in process_chunk(&mut parser, &first) {
            yield event;
        }

        loop {
            let next = tokio::time::timeout(streaming_read_timeout, byte_stream.next())
                .await
                .map_err(|_| GatewayError::StreamReadTimeout(streaming_read_timeout))?;
            let Some(chunk) = next else { break; };
            let chunk = chunk.map_err(|error| network_error(&error))?;
            for event in process_chunk(&mut parser, &chunk) {
                yield event;
            }
        }

        for tool_call in parser.take_tool_calls() {
            yield KiroEvent::tool_use(tool_call);
        }
    }
}

/// Convenience wrapper around [`parse_kiro_stream`] that reads the two
/// timeouts from `config`.
///
/// # Examples
///
/// ```
/// use lanius_core::upstream::parse_kiro_stream_with_config;
/// use lanius_core::Config;
/// use futures_util::StreamExt;
///
/// # async fn example() {
/// let chunks = vec![Ok(bytes::Bytes::from(r#"{"content":"hi"}"#))];
/// let stream = futures_util::stream::iter(chunks);
/// let events = parse_kiro_stream_with_config(stream, &Config::default());
/// let events: Vec<_> = events.collect().await;
/// assert!(!events.is_empty());
/// # }
/// ```
pub fn parse_kiro_stream_with_config<S>(
    byte_stream: S,
    config: &Config,
) -> impl Stream<Item = Result<KiroEvent>> + use<S>
where
    S: Stream<Item = std::result::Result<Bytes, reqwest::Error>> + Send + Unpin + 'static,
{
    parse_kiro_stream(
        byte_stream,
        config.first_token_timeout,
        config.streaming_read_timeout,
    )
}

/// Fully drains [`parse_kiro_stream`] into a single [`StreamResult`]:
/// content and thinking text are concatenated in arrival order into their
/// respective `result` fields (only regular content is scanned for
/// bracket-style tool calls), the last thinking signature is kept,
/// tool-use events are collected, and
/// only a truthy usage value overwrites `result.usage` (a `null`/falsy
/// usage event is treated as "no information" rather than clearing a
/// previously-seen value). After the stream ends,
/// [`parse_bracket_tool_calls`] is run over the full concatenated content
/// and any matches are merged in and deduplicated together with the
/// structured tool calls via [`deduplicate_tool_calls`] — this catches
/// bracket-style calls that Kiro emitted as plain text rather than
/// structured tool-call events.
///
/// # Examples
///
/// ```
/// use lanius_core::upstream::collect_stream_to_result;
/// use std::time::Duration;
///
/// # async fn example() {
/// let chunks = vec![Ok(bytes::Bytes::from(r#"{"content":"hi"}"#))];
/// let stream = futures_util::stream::iter(chunks);
/// let result = collect_stream_to_result(
///     stream, Duration::from_secs(5), Duration::from_secs(5),
/// ).await.expect("stream should collect");
/// assert_eq!(result.content, "hi");
/// # }
/// ```
pub async fn collect_stream_to_result<S>(
    byte_stream: S,
    first_token_timeout: Duration,
    streaming_read_timeout: Duration,
) -> Result<StreamResult>
where
    S: Stream<Item = std::result::Result<Bytes, reqwest::Error>> + Send + Unpin + 'static,
{
    let mut result = StreamResult::new();
    let stream = parse_kiro_stream(byte_stream, first_token_timeout, streaming_read_timeout);
    futures_util::pin_mut!(stream);
    while let Some(event) = stream.next().await {
        match event? {
            KiroEvent {
                event_type: KiroEventType::Content,
                content: Some(content),
                ..
            } => {
                result.content.push_str(&content);
            }
            KiroEvent {
                event_type: KiroEventType::Thinking,
                thinking_content,
                thinking_signature,
                ..
            } => {
                if let Some(thinking) = thinking_content {
                    result.thinking_content.push_str(&thinking);
                }
                if thinking_signature.is_some() {
                    result.thinking_signature = thinking_signature;
                }
            }
            KiroEvent {
                event_type: KiroEventType::ToolUse,
                tool_use: Some(tool_call),
                ..
            } => {
                result.tool_calls.push(tool_call);
            }
            KiroEvent {
                event_type: KiroEventType::Usage,
                usage: Some(usage),
                ..
            } => {
                if is_truthy(&usage) {
                    result.usage = Some(usage);
                }
            }
            KiroEvent {
                event_type: KiroEventType::ContextUsage,
                context_usage_percentage: Some(percentage),
                ..
            } => {
                result.context_usage_percentage = Some(percentage);
            }
            _ => {}
        }
    }
    let bracket_calls = parse_bracket_tool_calls(&result.content);
    if !bracket_calls.is_empty() {
        result.tool_calls.extend(bracket_calls);
        result.tool_calls = deduplicate_tool_calls(std::mem::take(&mut result.tool_calls));
    }
    Ok(result)
}

/// Convenience wrapper around [`collect_stream_to_result`] that reads the
/// timeouts from `config`, mirroring [`parse_kiro_stream_with_config`].
///
/// # Examples
///
/// ```
/// use lanius_core::upstream::collect_stream_to_result_with_config;
/// use lanius_core::Config;
///
/// # async fn example() {
/// let chunks = vec![Ok(bytes::Bytes::from(r#"{"content":"hi"}"#))];
/// let stream = futures_util::stream::iter(chunks);
/// let result = collect_stream_to_result_with_config(stream, &Config::default())
///     .await
///     .expect("stream should collect");
/// assert_eq!(result.content, "hi");
/// # }
/// ```
pub async fn collect_stream_to_result_with_config<S>(
    byte_stream: S,
    config: &Config,
) -> Result<StreamResult>
where
    S: Stream<Item = std::result::Result<Bytes, reqwest::Error>> + Send + Unpin + 'static,
{
    collect_stream_to_result(
        byte_stream,
        config.first_token_timeout,
        config.streaming_read_timeout,
    )
    .await
}

// Feeds one raw chunk through the AWS event-stream parser and maps each
// resulting parser event onto a `KiroEvent`. Tool calls are not emitted here;
// the caller drains them once the stream ends.
fn process_chunk(parser: &mut AwsEventStreamParser, chunk: &[u8]) -> Vec<KiroEvent> {
    parser
        .feed(chunk)
        .into_iter()
        .map(|event| match event {
            ParserEvent::Content(content) => KiroEvent::content(json_content_to_string(content)),
            ParserEvent::Reasoning { text, signature } => KiroEvent::thinking(text, signature),
            ParserEvent::Usage(usage) => KiroEvent::usage(usage),
            ParserEvent::ContextUsage(usage) => KiroEvent::context_usage(usage.as_f64()),
        })
        .collect()
}

// Content events are usually plain strings, but Kiro can in principle send
// non-string JSON content; render anything else via the crate's spaced-JSON
// formatter rather than dropping it, and treat `null` as empty text.
fn json_content_to_string(content: Value) -> String {
    match content {
        Value::String(content) => content,
        Value::Null => String::new(),
        other => crate::utils::format_json_spaced(&other),
    }
}

// Truthiness for a JSON value (see also `parser::is_truthy`), used here so a
// `null`/`0`/empty usage payload doesn't overwrite a previously-recorded
// real usage value.
fn is_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(number) => number.as_f64().map(|value| value != 0.0).unwrap_or(true),
        Value::String(value) => !value.is_empty(),
        Value::Array(value) => !value.is_empty(),
        Value::Object(value) => !value.is_empty(),
    }
}

fn network_error(error: &reqwest::Error) -> GatewayError {
    let info = classify_network_error(error);
    tracing::warn!(
        category = %info.category,
        details = %info.technical_details,
        "upstream stream read failed"
    );
    GatewayError::Network(Box::new(info))
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::stream;

    fn input(
        chunks: &[&[u8]],
    ) -> impl Stream<Item = std::result::Result<Bytes, reqwest::Error>> + Unpin + Send + 'static + use<>
    {
        let chunks: Vec<_> = chunks
            .iter()
            .map(|chunk| Bytes::copy_from_slice(chunk))
            .collect();
        stream::iter(chunks.into_iter().map(Ok))
    }

    async fn events(chunks: &[&[u8]]) -> Vec<KiroEvent> {
        let items: Vec<_> = parse_kiro_stream(
            input(chunks),
            Duration::from_secs(1),
            Duration::from_secs(1),
        )
        .collect()
        .await;
        items
            .into_iter()
            .collect::<Result<Vec<_>>>()
            .unwrap_or_default()
    }

    #[tokio::test]
    async fn parses_content_usage_context_and_tools_at_eof() {
        let payload = br#"{"content":"Hello"}{"usage":{"credits":1}}{"contextUsagePercentage":7.5}{"name":"f","toolUseId":"call_1","input":{"x":1},"stop":true}"#;
        let got = events(&[payload]).await;
        assert!(matches!(got[0].event_type, KiroEventType::Content));
        assert!(matches!(got[1].event_type, KiroEventType::Usage));
        assert!(matches!(got[2].event_type, KiroEventType::ContextUsage));
        assert!(matches!(got[3].event_type, KiroEventType::ToolUse));
        assert_eq!(
            got[3].tool_use.as_ref().map(|tool| tool.name.as_str()),
            Some("f")
        );
    }

    #[tokio::test]
    async fn empty_response_is_successful() {
        let got = events(&[]).await;
        assert!(got.is_empty());
    }

    #[tokio::test]
    async fn native_reasoning_events_become_thinking_and_tags_stay_content() {
        let got = events(&[
            br#"{"text":"reason"}{"text":"ing"}"#,
            br#"{"signature":"sig"}{"content":"<thinking>x</thinking>answer"}"#,
        ])
        .await;
        assert_eq!(got[0].thinking_content.as_deref(), Some("reason"));
        assert_eq!(got[1].thinking_content.as_deref(), Some("ing"));
        assert_eq!(got[2].event_type, KiroEventType::Thinking);
        assert_eq!(got[2].thinking_signature.as_deref(), Some("sig"));
        assert_eq!(
            got[3].content.as_deref(),
            Some("<thinking>x</thinking>answer"),
            "inline tags are no longer parsed as thinking"
        );

        let result = collect_stream_to_result(
            input(&[br#"{"text":"[Called f with args: {}]"}{"signature":"s"}{"content":"ok"}"#]),
            Duration::from_secs(1),
            Duration::from_secs(1),
        )
        .await
        .unwrap_or_default();
        assert_eq!(result.thinking_content, "[Called f with args: {}]");
        assert_eq!(result.thinking_signature.as_deref(), Some("s"));
        assert_eq!(result.content, "ok");
        assert!(
            result.tool_calls.is_empty(),
            "thinking text is not scanned for tool calls"
        );
    }

    #[tokio::test]
    async fn collects_and_ignores_null_context_usage() {
        let result = collect_stream_to_result(
            input(&[br#"{"content":"A"}{"contextUsagePercentage":4}{"contextUsagePercentage":null}{"usage":{}}"#]),
            Duration::from_secs(1),
            Duration::from_secs(1),
        )
        .await
        .unwrap_or_default();
        assert_eq!(result.content, "A");
        assert_eq!(result.context_usage_percentage, Some(4.0));
        assert_eq!(result.usage, None);
    }

    #[tokio::test]
    async fn bracket_calls_are_merged_after_collection() {
        let result = collect_stream_to_result(
            input(&[br#"{"content":"[Called f with args: {\"a\": 1}]"}"#]),
            Duration::from_secs(1),
            Duration::from_secs(1),
        )
        .await
        .unwrap_or_default();
        assert_eq!(result.tool_calls.len(), 1);
        assert_eq!(result.tool_calls[0].name, "f");
    }
}
