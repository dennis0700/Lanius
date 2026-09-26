//! Anthropic Messages API compatibility layer.
//!
//! Implements the Anthropic `/v1/messages` and `/v1/messages/count_tokens`
//! endpoints on top of the shared Kiro upstream. Requests are validated and
//! converted to the Kiro payload format via [`crate::convert::anthropic`],
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

pub(crate) mod models;
pub(crate) mod routes;
pub(crate) mod sse;

pub use models::{
    AnthropicMessage, AnthropicMessageContent, AnthropicMessageRole, AnthropicMessagesRequest,
    AnthropicTool, SystemPrompt,
};
pub use routes::{AnthropicState, router};
