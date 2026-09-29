//! Converts OpenAI/Anthropic-shaped chat requests into the Kiro backend's request payload
//! format, and provides the shared abstractions and safety nets used along the way.
//!
//! This is the "shape translation" layer of the gateway, distinct from [`crate::auth`]
//! (credentials) and [`crate::compat`] (host/tool-name/model-id quirks — though this
//! module does call into [`crate::compat::ToolNameAliases`] to alias tool names before
//! they reach Kiro). The submodules are:
//!
//! - `core` — the provider-agnostic core: a [`core::UnifiedMessage`]/[`core::UnifiedTool`]
//!   intermediate representation that both provider-specific converters below produce, and
//!   [`core::build_kiro_payload`], which normalizes that representation (merging adjacent
//!   same-role messages, ensuring the conversation starts with a user turn and alternates
//!   roles, attaching native reasoning fields when the model supports them, handling tool
//!   calls/results) into the final Kiro `conversationState` JSON payload.
//! - `openai` — converts an OpenAI-style `ChatCompletionRequest`
//!   (`crate::api` OpenAI models) into the unified representation and then into a Kiro
//!   payload via [`core::build_kiro_payload`].
//! - `anthropic` — the equivalent for an Anthropic-style `AnthropicMessagesRequest`
//!   (`crate::api` Anthropic models).
//! - `guards` — protective post-processing applied to the fully built Kiro payload:
//!   trimming conversation history to fit a byte budget, and repairing tool-result entries
//!   that would otherwise reference a tool call that got trimmed away (which Kiro would
//!   reject as an "orphaned" tool result).
//!
//! Call sites (the `api::openai`/`api::anthropic` route handlers) call `openai::build_kiro_payload`
//! or `anthropic::anthropic_to_kiro` to get a [`core::KiroPayloadResult`] ready to send
//! upstream to Kiro.

mod anthropic;
mod core;
mod guards;
mod openai;

pub use anthropic::{
    anthropic_to_kiro, convert_anthropic_content_to_text, convert_anthropic_messages,
    convert_anthropic_tools, extract_system_prompt, reasoning_request_from_anthropic,
};
pub use core::{
    KiroPayloadInput, KiroPayloadResult, UnifiedImage, UnifiedMessage, UnifiedTool,
    build_kiro_history, build_kiro_payload as build_kiro_payload_from_unified,
    convert_tools_to_kiro_format, ensure_alternating_roles, ensure_first_message_is_user,
    extract_images_from_content, extract_text_content, merge_adjacent_messages,
    normalize_message_roles, process_tools_with_long_descriptions, sanitize_json_schema,
    tool_calls_to_text, tool_results_to_text, validate_tool_names,
};
pub use guards::{PayloadTrimStats, check_payload_size, trim_payload_to_limit};
pub use openai::{
    build_kiro_payload, convert_openai_messages_to_unified, convert_openai_tools_to_unified,
    reasoning_request_from_openai,
};
