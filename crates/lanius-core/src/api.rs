//! HTTP-facing API surface of the Lanius gateway.
//!
//! Lanius proxies requests from clients speaking either the Anthropic
//! Messages API or the OpenAI Chat Completions API, converts them into the
//! Kiro (Amazon Q Developer / AWS CodeWhisperer) wire format, forwards them
//! to the upstream Kiro service via [`crate::upstream`], and translates the
//! upstream response/stream back into the client's expected protocol.
//!
//! This module is a thin namespace that groups the two protocol
//! implementations:
//! - [`anthropic`]: routes, request/response models, and SSE encoding for
//!   the Anthropic Messages API (`/v1/messages`, `/v1/messages/count_tokens`).
//! - [`openai`]: routes, request/response models, and SSE encoding for the
//!   OpenAI Chat Completions API (`/v1/chat/completions`, `/v1/models`).
//!
//! Both submodules share cross-cutting infrastructure from
//! [`crate::auth`] (token refresh), [`crate::convert`] (protocol-to-Kiro
//! payload conversion), and [`crate::truncation`] (recovery from upstream
//! output truncation).

pub(crate) mod anthropic;
pub(crate) mod openai;

pub use anthropic::sse::{
    AnthropicSseFormatter, RequestTokenInput, format_sse_event as anthropic_format_sse_event,
    generate_message_id as anthropic_generate_message_id,
    generate_thinking_signature as anthropic_generate_thinking_signature,
    response_from_stream_result,
};
pub use anthropic::{
    AnthropicMessage, AnthropicMessageContent, AnthropicMessageRole, AnthropicMessagesRequest,
    AnthropicState, AnthropicTool, SystemPrompt, router as anthropic_router,
};
pub use openai::sse::{OpenAiFormatContext, collect_openai_response, encode_openai_sse};
pub use openai::{
    ChatCompletionRequest, ChatMessage, OpenAIMessageContent, OpenAiState, ReasoningEffort, Tool,
    ToolFunction, router as openai_router,
};
