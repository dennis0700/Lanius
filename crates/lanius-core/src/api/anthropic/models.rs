//! Serde wire types for the Anthropic Messages API.
//!
//! These types mirror the JSON shapes accepted and produced by Anthropic's
//! `/v1/messages` and `/v1/messages/count_tokens` endpoints. They are used
//! by [`super::routes`] to deserialize incoming client requests and by
//! [`super::sse`] to build both the non-streaming response body and the
//! individual Server-Sent Events frames of a streaming response.
//!
//! Most content-block variants are deliberately permissive: unrecognized
//! block "type" values fall back to [`ContentBlock::Unknown`] (or
//! [`ToolResultContent::Other`] / [`SystemPrompt::RawBlocks`]) rather than
//! failing deserialization, so that clients sending newer Anthropic
//! protocol features do not break the gateway outright.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Generates a unit-like enum with a single variant that always
/// (de)serializes as the given string literal (e.g. Anthropic's
/// discriminating `"type": "text"` field). This lets each content-block
/// struct carry a `kind` field that round-trips a fixed tag value without
/// hand-writing a `Serialize`/`Deserialize`/`Default` impl for every block
/// type.
macro_rules! block_type {
    ($name:ident, $value:literal) => {
        #[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
        #[serde(rename_all = "snake_case")]
        pub enum $name {
            #[serde(rename = $value)]
            Value,
        }

        impl Default for $name {
            fn default() -> Self {
                Self::Value
            }
        }
    };
}

block_type!(TextBlockType, "text");
block_type!(ThinkingBlockType, "thinking");
block_type!(ToolUseBlockType, "tool_use");
block_type!(ToolResultBlockType, "tool_result");
block_type!(ToolReferenceBlockType, "tool_reference");
block_type!(ImageBlockType, "image");
block_type!(ServerToolUseBlockType, "server_tool_use");
block_type!(WebSearchToolResultBlockType, "web_search_tool_result");

/// A plain text content block (`{"type": "text", "text": "..."}`).
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct TextContentBlock {
    #[serde(rename = "type", default)]
    pub kind: TextBlockType,
    pub text: String,
}

/// An extended-thinking content block, carrying the model's chain-of-thought
/// text and its opaque `signature` (used by Anthropic to verify the
/// thinking block was not tampered with when replayed in later turns).
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct ThinkingContentBlock {
    #[serde(rename = "type", default)]
    pub kind: ThinkingBlockType,
    pub thinking: String,
    #[serde(default)]
    pub signature: String,
}

/// A tool invocation emitted by the assistant, with the tool's `name` and
/// `input` arguments (parsed as a JSON object).
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct ToolUseContentBlock {
    #[serde(rename = "type", default)]
    pub kind: ToolUseBlockType,
    pub id: String,
    pub name: String,
    pub input: Map<String, Value>,
}

/// A reference to a deferred/lazily-loaded tool definition, identified only
/// by `tool_name` (used by clients like Claude Code for MCP tools that are
/// not inlined into every request).
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct ToolReferenceContentBlock {
    #[serde(rename = "type", default)]
    pub kind: ToolReferenceBlockType,
    pub tool_name: String,
}

/// An inline base64-encoded image source (`{"type": "base64", ...}`).
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct Base64ImageSource {
    #[serde(rename = "type", default = "default_base64_type")]
    pub kind: String,
    pub media_type: String,
    pub data: String,
}

fn default_base64_type() -> String {
    "base64".to_owned()
}

/// A remote image source referenced by URL (`{"type": "url", "url": ...}`).
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct UrlImageSource {
    #[serde(rename = "type", default = "default_url_type")]
    pub kind: String,
    pub url: String,
}

fn default_url_type() -> String {
    "url".to_owned()
}

/// The source of an [`ImageContentBlock`]: either inline base64 data, a
/// URL, or an unrecognized shape preserved verbatim as [`Value`].
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(untagged)]
pub enum ImageSource {
    Base64(Base64ImageSource),
    Url(UrlImageSource),
    Unknown(Value),
}

/// An image content block, as sent in multimodal user messages.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct ImageContentBlock {
    #[serde(rename = "type", default)]
    pub kind: ImageBlockType,
    pub source: ImageSource,
}

/// The `content` payload of a [`ToolResultContentBlock`]: a plain string,
/// a list of nested content blocks (e.g. text plus tool references), or an
/// arbitrary JSON value for shapes not otherwise modeled.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(untagged)]
pub enum ToolResultContent {
    Text(String),
    Blocks(Vec<ContentBlock>),
    Other(Value),
}

/// The result of a previously requested tool call, matched back to its
/// invocation via `tool_use_id`. `is_error` marks that the tool execution
/// failed and its `content` should be treated as an error message by the
/// model.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct ToolResultContentBlock {
    #[serde(rename = "type", default)]
    pub kind: ToolResultBlockType,
    pub tool_use_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<ToolResultContent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
}

/// A tool invocation performed by Anthropic's own server-side tools (e.g.
/// web search), as opposed to a client-defined tool.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct ServerToolUseContentBlock {
    #[serde(rename = "type", default)]
    pub kind: ServerToolUseBlockType,
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub input: Map<String, Value>,
}

/// The result of a server-side web search tool invocation.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct WebSearchToolResultContentBlock {
    #[serde(rename = "type", default)]
    pub kind: WebSearchToolResultBlockType,
    pub tool_use_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<Value>,
}

/// A content block whose `"type"` was not recognized by any known variant.
/// Preserving it as raw JSON (rather than rejecting the request) keeps the
/// gateway forward-compatible with newer Anthropic protocol features.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(transparent)]
pub struct UnknownContentBlock(pub Value);

/// A single element of a message's content array. Variants are tried in
/// declaration order (via `#[serde(untagged)]`); [`ContentBlock::Unknown`]
/// is the catch-all fallback for any block shape that does not match a
/// known variant.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(untagged)]
pub enum ContentBlock {
    Text(TextContentBlock),
    Thinking(ThinkingContentBlock),
    Image(ImageContentBlock),
    ToolUse(ToolUseContentBlock),
    ToolResult(ToolResultContentBlock),
    ToolReference(ToolReferenceContentBlock),
    ServerToolUse(ServerToolUseContentBlock),
    WebSearchToolResult(WebSearchToolResultContentBlock),
    Unknown(UnknownContentBlock),
}

/// The role of a message in an Anthropic conversation. `System` is only
/// used for inline system-reminder messages embedded in the `messages`
/// array (as opposed to the top-level `system` field).
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AnthropicMessageRole {
    /// A message sent by the end user.
    User,
    /// A message generated by the model.
    Assistant,
    /// An inline system-reminder message embedded in `messages`.
    System,
}

/// A message's `content`: either a plain string shorthand or a full array
/// of content blocks.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(untagged)]
pub enum AnthropicMessageContent {
    /// Plain string shorthand content.
    Text(String),
    /// A full array of typed content blocks.
    Blocks(Vec<ContentBlock>),
}

/// A single turn in the conversation history sent to `/v1/messages`.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct AnthropicMessage {
    /// Who sent this turn.
    pub role: AnthropicMessageRole,
    /// The turn's content.
    pub content: AnthropicMessageContent,
}

/// A tool definition offered to the model. `kind` distinguishes
/// Anthropic-hosted server tools (e.g. `"web_search_20250305"`) from
/// user-defined tools (`kind` absent); see the custom [`Deserialize`] impl
/// below for the validation this distinction enables.
#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct AnthropicTool {
    /// Present (e.g. `"web_search_20250305"`) only for Anthropic-hosted
    /// server tools; absent for user-defined tools.
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// The tool's name, as referenced by tool-use/tool-result blocks.
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Human-readable description shown to the model.
    pub description: Option<String>,
    /// JSON Schema describing the tool's input, required for user-defined
    /// tools.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_schema: Option<Map<String, Value>>,
    /// Server-tool-only: maximum number of times the model may invoke this
    /// tool in one turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_uses: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Server-tool-only (e.g. web search): domains the tool may access.
    pub allowed_domains: Option<Vec<String>>,
    /// Server-tool-only (e.g. web search): domains the tool must not access.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_domains: Option<Vec<String>>,
    /// Server-tool-only (e.g. web search): the user's location context.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_location: Option<Map<String, Value>>,
}

/// Wire-format mirror of [`AnthropicTool`] used only during deserialization
/// so the custom `Deserialize` impl can inspect `kind`/`input_schema`
/// before deciding whether to accept the tool.
#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
struct AnthropicToolWire {
    #[serde(rename = "type", default)]
    kind: Option<String>,
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    input_schema: Option<Map<String, Value>>,
    #[serde(default)]
    max_uses: Option<i64>,
    #[serde(default)]
    allowed_domains: Option<Vec<String>>,
    #[serde(default)]
    blocked_domains: Option<Vec<String>>,
    #[serde(default)]
    user_location: Option<Map<String, Value>>,
}

impl<'de> Deserialize<'de> for AnthropicTool {
    /// Deserializes a tool definition, rejecting user-defined tools (those
    /// with no `"type"` field) that omit `input_schema` — Anthropic requires
    /// user tools to declare their input shape, while server-hosted tools
    /// (which do have a `"type"`) are allowed to omit it.
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let tool = AnthropicToolWire::deserialize(deserializer)?;
        if tool.kind.is_none() && tool.input_schema.is_none() {
            return Err(serde::de::Error::custom(
                "input_schema is required for user-defined tools (those without a 'type' field)",
            ));
        }
        Ok(Self {
            kind: tool.kind,
            name: tool.name,
            description: tool.description,
            input_schema: tool.input_schema,
            max_uses: tool.max_uses,
            allowed_domains: tool.allowed_domains,
            blocked_domains: tool.blocked_domains,
            user_location: tool.user_location,
        })
    }
}

/// Tag type for [`ToolChoiceAuto`] (model decides whether/which tool to use).
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolChoiceAutoType {
    #[default]
    Auto,
}

/// `tool_choice: {"type": "auto"}` — the model decides freely whether to
/// call a tool.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub struct ToolChoiceAuto {
    #[serde(rename = "type", default)]
    pub kind: ToolChoiceAutoType,
}

/// Tag type for [`ToolChoiceAny`] (model must use some tool).
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolChoiceAnyType {
    #[default]
    Any,
}

/// `tool_choice: {"type": "any"}` — the model must call one of the offered
/// tools, but may choose which.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub struct ToolChoiceAny {
    #[serde(rename = "type", default)]
    pub kind: ToolChoiceAnyType,
}

/// Tag type for [`ToolChoiceTool`] (model must use a specific named tool).
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolChoiceToolType {
    #[default]
    Tool,
}

/// `tool_choice: {"type": "tool", "name": "..."}` — the model must call the
/// named tool.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub struct ToolChoiceTool {
    #[serde(rename = "type", default)]
    pub kind: ToolChoiceToolType,
    pub name: String,
}

/// The request's `tool_choice` setting. `Other` preserves any shape not
/// recognized by the known variants (forward compatibility).
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(untagged)]
pub enum ToolChoice {
    /// The model decides freely whether to call a tool.
    Auto(ToolChoiceAuto),
    /// The model must call one of the offered tools.
    Any(ToolChoiceAny),
    /// The model must call the named tool.
    Tool(ToolChoiceTool),
    /// Any shape not recognized by the known variants.
    Other(Value),
}

/// A single block within a structured (array-form) `system` prompt.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct SystemContentBlock {
    #[serde(rename = "type", default)]
    pub kind: TextBlockType,
    /// The block's text.
    pub text: String,
    /// Anthropic prompt-caching directive for this block, if requested.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<Map<String, Value>>,
}

/// The request's `system` prompt: a plain string, an array of typed system
/// blocks, or (as a compatibility fallback) an array of raw JSON values for
/// system block shapes this gateway does not model.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(untagged)]
pub enum SystemPrompt {
    /// Plain string shorthand.
    Text(String),
    /// A structured array of typed system blocks.
    Blocks(Vec<SystemContentBlock>),
    /// Raw JSON blocks not matching any known system block shape.
    RawBlocks(Vec<Value>),
}

/// The full body of a non-streaming `/v1/messages` request.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct AnthropicMessagesRequest {
    /// The target model id.
    pub model: String,
    /// The conversation history.
    pub messages: Vec<AnthropicMessage>,
    /// Maximum number of tokens to generate.
    pub max_tokens: i64,
    /// The system prompt, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<SystemPrompt>,
    /// Whether to stream the response as Server-Sent Events.
    #[serde(default)]
    pub stream: bool,
    /// Anthropic's `thinking` config (e.g. budget/type), used to enable
    /// native extended thinking on models that support it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<Value>,
    /// Anthropic's `output_config` (e.g. `{"effort": "high"}`), used to pick
    /// the native reasoning effort on models that support it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_config: Option<Value>,
    /// Tools offered to the model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<AnthropicTool>>,
    /// Controls whether/which tool the model must call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoice>,
    /// Sampling temperature in `[0.0, 1.0]`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_unit_interval"
    )]
    pub temperature: Option<f64>,
    /// Nucleus sampling threshold in `[0.0, 1.0]`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_unit_interval"
    )]
    pub top_p: Option<f64>,
    /// Top-k sampling cutoff (non-negative).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_nonnegative_i64"
    )]
    pub top_k: Option<i64>,
    /// Sequences that stop generation when encountered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_sequences: Option<Vec<String>>,
    /// Opaque client metadata (e.g. `user_id`), passed through unused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Map<String, Value>>,
}

/// Rejects `temperature`/`top_p` values outside `[0.0, 1.0]` (or
/// non-finite), matching Anthropic's accepted sampling range.
fn deserialize_unit_interval<'de, D>(deserializer: D) -> Result<Option<f64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<f64>::deserialize(deserializer)?;
    if value.is_some_and(|number| !number.is_finite() || !(0.0..=1.0).contains(&number)) {
        return Err(serde::de::Error::custom(
            "value must be a finite number between 0 and 1",
        ));
    }
    Ok(value)
}

/// Rejects negative `top_k` values.
fn deserialize_nonnegative_i64<'de, D>(deserializer: D) -> Result<Option<i64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<i64>::deserialize(deserializer)?;
    if value.is_some_and(|number| number < 0) {
        return Err(serde::de::Error::custom(
            "value must be greater than or equal to 0",
        ));
    }
    Ok(value)
}

/// The body of a `/v1/messages/count_tokens` request — the same
/// conversational shape as [`AnthropicMessagesRequest`] but without
/// generation parameters, since only token estimation is performed.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct AnthropicCountTokensRequest {
    pub model: String,
    pub messages: Vec<AnthropicMessage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<SystemPrompt>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<AnthropicTool>>,
}

fn default_error_type() -> String {
    "error".to_owned()
}

/// The `error` object nested inside [`AnthropicErrorResponse`].
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct AnthropicErrorDetail {
    #[serde(rename = "type")]
    pub kind: String,
    pub message: String,
}

/// The JSON error envelope returned to clients for both non-streaming
/// error responses and the payload of a streaming `error` SSE event.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct AnthropicErrorResponse {
    #[serde(rename = "type", default = "default_error_type")]
    pub kind: String,
    pub error: AnthropicErrorDetail,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn real_claude_code_history_accepts_inline_system_server_and_unknown_blocks() {
        let request: AnthropicMessagesRequest = serde_json::from_value(json!({
            "model": "claude-sonnet-4-5",
            "max_tokens": 4096,
            "system": [{"type": "text", "text": "outer system", "cache_control": {"type": "ephemeral"}}],
            "messages": [
                {"role": "system", "content": [{"type": "text", "text": "<system-reminder>keep this</system-reminder>"}]},
                {"role": "assistant", "content": [
                    {"type": "server_tool_use", "id": "srvtoolu_1", "name": "web_search", "input": {"query": "Kiro"}}
                ]},
                {"role": "user", "content": [
                    {"type": "web_search_tool_result", "tool_use_id": "srvtoolu_1", "content": [{"type": "web_search_result", "title": "Kiro"}]},
                    {"type": "future_tool_result", "opaque": {"version": 2}},
                    {"type": "tool_result", "tool_use_id": "toolu_1", "content": [
                        {"type": "text", "text": "Loaded deferred tool"},
                        {"type": "tool_reference", "tool_name": "mcp__slack__read_channel"}
                    ]}
                ]}
            ]
        }))
        .expect("real Claude Code compatibility fixture must deserialize");

        assert_eq!(request.messages[0].role, AnthropicMessageRole::System);
        let AnthropicMessageContent::Blocks(blocks) = &request.messages[2].content else {
            panic!("fixture has block content");
        };
        assert!(matches!(blocks[0], ContentBlock::WebSearchToolResult(_)));
        assert!(matches!(blocks[1], ContentBlock::Unknown(_)));
        let ContentBlock::ToolResult(result) = &blocks[2] else {
            panic!("fixture has tool result");
        };
        assert!(matches!(result.content, Some(ToolResultContent::Blocks(_))));
    }

    #[test]
    fn system_accepts_text_and_raw_block_arrays() {
        let text: SystemPrompt = serde_json::from_value(json!("be concise")).expect("text system");
        let raw: SystemPrompt = serde_json::from_value(json!([
            {"type": "future_system_block", "payload": true}
        ]))
        .expect("raw system blocks");
        assert!(matches!(text, SystemPrompt::Text(_)));
        assert!(matches!(raw, SystemPrompt::RawBlocks(_)));
    }

    #[test]
    fn optional_fields_do_not_emit_null() {
        let tool = AnthropicTool {
            kind: None,
            name: "a_tool".to_owned(),
            description: None,
            input_schema: Some(Map::new()),
            max_uses: None,
            allowed_domains: None,
            blocked_domains: None,
            user_location: None,
        };
        assert_eq!(
            serde_json::to_value(tool).expect("serialize tool"),
            json!({
                "name": "a_tool", "input_schema": {}
            })
        );
    }
}

#[cfg(test)]
mod validation_regression_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn user_tools_require_input_schema_but_server_tools_do_not() {
        let user_tool = serde_json::from_value::<AnthropicTool>(json!({"name": "search"}));
        assert!(user_tool.is_err());
        let server_tool = serde_json::from_value::<AnthropicTool>(json!({
            "type": "web_search_20250305", "name": "web_search"
        }));
        assert!(server_tool.is_ok());
    }

    #[test]
    fn sampling_parameters_enforce_valid_ranges() {
        for invalid in [
            json!({"temperature": -0.01}),
            json!({"temperature": 1.01}),
            json!({"top_p": -0.01}),
            json!({"top_p": 1.01}),
            json!({"top_k": -1}),
        ] {
            let request = json!({
                "model": "claude", "max_tokens": 1, "messages": [],
                "temperature": invalid.get("temperature").cloned(),
                "top_p": invalid.get("top_p").cloned(),
                "top_k": invalid.get("top_k").cloned(),
            });
            assert!(serde_json::from_value::<AnthropicMessagesRequest>(request).is_err());
        }
        let valid = serde_json::from_value::<AnthropicMessagesRequest>(json!({
            "model": "claude", "max_tokens": 1, "messages": [],
            "temperature": 0, "top_p": 1, "top_k": 0
        }));
        assert!(valid.is_ok());
    }
}
