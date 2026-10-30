//! OpenAI Responses API (`POST /v1/responses`).
//!
//! Requests are converted straight to the Kiro payload by
//! [`crate::convert::responses_to_kiro`] — not translated into a Chat
//! Completions request first — and Kiro's stream is formatted as Responses
//! output items / semantic SSE events.
//!
//! Lanius stores nothing, so `previous_response_id` and `item_reference`
//! inputs are rejected and `store` is always reported as `false`; clients
//! must send the full conversation in `input` (as e.g. Codex does).
//!
//! Submodules:
//! - [`models`]: request wire types.
//! - [`routes`]: the axum handler (auth, failover, truncation recovery,
//!   server-side `web_search`).
//! - [`sse`]: output item assembly and SSE event encoding.

mod models;
mod routes;
mod sse;

pub use models::{ResponsesInput, ResponsesReasoning, ResponsesRequest, ResponsesTool};
pub(super) use routes::create_response;
