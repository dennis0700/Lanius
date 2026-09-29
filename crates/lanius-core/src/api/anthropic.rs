//! Anthropic Messages API compatibility layer.
//!
//! Implements the Anthropic `/v1/messages` and `/v1/messages/count_tokens`
//! endpoints on top of the shared Kiro upstream. Requests are validated and
//! converted to the Kiro payload format via [`crate::convert::anthropic_to_kiro`],
//! and Kiro's response/stream is translated back into Anthropic-shaped
//! JSON or Server-Sent Events.
//!
//! Submodules:
//! - [`models`]: serde types for the Anthropic wire protocol (messages,
//!   content blocks, tools, streaming events).
//! - [`routes`]: axum route handlers, account selection/failover, header
//!   authentication, and truncation-recovery request rewriting.
//! - [`sse`]: the streaming state machine that turns Kiro's internal event
//!   stream into Anthropic SSE frames, plus non-streaming response assembly.

mod models;
mod routes;
mod sse;

pub use models::{
    AnthropicMessage, AnthropicMessageContent, AnthropicMessageRole, AnthropicMessagesRequest,
    AnthropicTool, ContentBlock, ImageSource, SystemPrompt, ToolResultContent,
};
#[cfg(test)]
pub(crate) use models::{TextContentBlock, ToolResultContentBlock, ToolUseContentBlock};
pub use routes::{AnthropicState, router};
pub use sse::{
    AnthropicSseFormatter, DEFAULT_PING_INTERVAL, RequestTokenInput, format_sse_event,
    generate_message_id, generate_thinking_signature, response_from_stream_result,
};
