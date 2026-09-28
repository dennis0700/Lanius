//! Kiro upstream HTTP client, AWS event-stream parsing, and unified stream
//! events.
//!
//! [`KiroHttpClient`] (in [`client`]) sends converted requests to Kiro over
//! HTTP with retry/backoff and token refresh. [`parser`] decodes Kiro's raw
//! AWS event-stream response bytes (including bracket-style
//! `[Called ... with args: {...}]` tool calls and native
//! `reasoningContentEvent` thinking) into structured events. [`stream`]
//! wraps the parser to produce the crate-wide [`KiroEvent`] stream that the
//! API route handlers in [`crate::api`] convert into OpenAI/Anthropic-shaped
//! responses.

pub(crate) mod client;
pub(crate) mod endpoint;
pub(crate) mod parser;
pub(crate) mod stream;

pub use client::KiroHttpClient;
pub use parser::{
    AwsEventStreamParser, ParserEvent, ToolCall, TruncationInfo, deduplicate_tool_calls,
    diagnose_json_truncation, find_matching_brace, parse_bracket_tool_calls,
};
pub use stream::{
    KiroEvent, KiroEventType, StreamResult, collect_stream_to_result,
    collect_stream_to_result_with_config, parse_kiro_stream, parse_kiro_stream_with_config,
};
