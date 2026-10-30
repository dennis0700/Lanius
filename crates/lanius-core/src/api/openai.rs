//! OpenAI-compatible API layer: Chat Completions and Responses.
//!
//! Implements the OpenAI-shaped `/v1/models`, `/v1/chat/completions` and
//! `/v1/responses` endpoints on top of the shared Kiro upstream. Requests are
//! validated, optionally augmented (e.g. server-executed `web_search` tool,
//! truncation recovery messages), and converted to the Kiro payload format via
//! [`crate::convert::build_kiro_payload`] (Chat Completions) or
//! [`crate::convert::responses_to_kiro`] (Responses). Kiro's response/stream is
//! translated back into OpenAI-shaped JSON or Server-Sent Events.
//!
//! Submodules:
//! - [`models`]: serde types for the OpenAI wire protocol (chat messages,
//!   tools, completion responses/chunks).
//! - [`routes`]: axum route handlers, account selection/failover, bearer
//!   token authentication, and model listing.
//! - [`sse`]: streaming encoder that turns Kiro's internal event stream
//!   into OpenAI `chat.completion.chunk` SSE frames, plus non-streaming
//!   response assembly.
//! - [`responses`]: the Responses API (`/v1/responses`) request types,
//!   handler and output/SSE encoding.

mod models;
mod responses;
mod routes;
mod sse;

pub use models::{
    ChatCompletionRequest, ChatMessage, OpenAIMessageContent, ReasoningEffort, Tool, ToolFunction,
};
pub use responses::{ResponsesInput, ResponsesReasoning, ResponsesRequest, ResponsesTool};
pub use routes::{OpenAiState, router};
pub use sse::{OpenAiFormatContext, collect_openai_response, encode_openai_sse};
