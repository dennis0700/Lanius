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
//! - `anthropic`: routes, request/response models, and SSE encoding for
//!   the Anthropic Messages API (`/v1/messages`, `/v1/messages/count_tokens`).
//! - `openai`: routes, request/response models, and SSE encoding for the
//!   OpenAI Chat Completions API (`/v1/chat/completions`, `/v1/models`).
//!
//! Both submodules share cross-cutting infrastructure from
//! [`crate::auth`] (token refresh), [`crate::convert`] (protocol-to-Kiro
//! payload conversion), and [`crate::truncation`] (recovery from upstream
//! output truncation).

mod anthropic;
mod openai;

use axum::response::Response;

/// Operator-facing diagnostic text attached to an error response as a
/// response extension. `crate::server`'s failed-response logging middleware
/// takes it out and logs it; it is never serialized to clients.
#[derive(Debug, Clone)]
pub(crate) struct ErrorDetail(String);

impl ErrorDetail {
    /// Attaches `detail` to `response` and returns the response.
    pub(crate) fn attach(mut response: Response, detail: impl Into<String>) -> Response {
        response.extensions_mut().insert(Self(detail.into()));
        response
    }

    /// Removes and returns the detail attached to `response`, if any.
    pub(crate) fn take(response: &mut Response) -> Option<String> {
        response
            .extensions_mut()
            .remove::<Self>()
            .map(|detail| detail.0)
    }
}

pub use anthropic::{
    AnthropicMessage, AnthropicMessageContent, AnthropicMessageRole, AnthropicMessagesRequest,
    AnthropicSseFormatter, AnthropicState, AnthropicTool, RequestTokenInput, SystemPrompt,
    format_sse_event as anthropic_format_sse_event,
    generate_message_id as anthropic_generate_message_id,
    generate_thinking_signature as anthropic_generate_thinking_signature,
    response_from_stream_result, router as anthropic_router,
};
pub(crate) use anthropic::{
    ContentBlock as AnthropicContentBlock, DEFAULT_PING_INTERVAL,
    ImageSource as AnthropicImageSource, ToolResultContent as AnthropicToolResultContent,
};
#[cfg(test)]
pub(crate) use anthropic::{
    TextContentBlock as AnthropicTextContentBlock,
    ToolResultContentBlock as AnthropicToolResultContentBlock,
    ToolUseContentBlock as AnthropicToolUseContentBlock,
};
pub use openai::{
    ChatCompletionRequest, ChatMessage, OpenAIMessageContent, OpenAiFormatContext, OpenAiState,
    ReasoningEffort, Tool, ToolFunction, collect_openai_response, encode_openai_sse,
    router as openai_router,
};
