//! OpenAI Chat Completions API compatibility layer.
//!
//! Implements the OpenAI-shaped `/v1/models` and `/v1/chat/completions`
//! endpoints on top of the shared Kiro upstream. Requests are validated,
//! optionally augmented (e.g. injected `web_search` tool, truncation
//! recovery messages), and converted to the Kiro payload format via
//! [`crate::convert::build_kiro_payload`]. Kiro's response/stream is translated back
//! into OpenAI-shaped JSON or Server-Sent Events chunks.
//!
//! Submodules:
//! - [`models`]: serde types for the OpenAI wire protocol (chat messages,
//!   tools, completion responses/chunks).
//! - [`routes`]: axum route handlers, account selection/failover, bearer
//!   token authentication, and model listing.
//! - [`sse`]: streaming encoder that turns Kiro's internal event stream
//!   into OpenAI `chat.completion.chunk` SSE frames, plus non-streaming
//!   response assembly.

mod models;
mod routes;
mod sse;

pub use models::{
    ChatCompletionRequest, ChatMessage, OpenAIMessageContent, ReasoningEffort, Tool, ToolFunction,
};
pub use routes::{OpenAiState, router};
pub use sse::{OpenAiFormatContext, collect_openai_response, encode_openai_sse};
