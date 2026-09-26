//! Serde wire types for the OpenAI Chat Completions API.
//!
//! Mirrors the JSON shapes accepted and produced by OpenAI's `/v1/models`
//! and `/v1/chat/completions` endpoints. Used by [`super::routes`] to
//! deserialize incoming requests and by [`super::sse`] to build both the
//! non-streaming response body and the individual `chat.completion.chunk`
//! Server-Sent Events frames of a streaming response.
//!
//! Several types accept more than one shape for backward/tooling
//! compatibility — e.g. [`OpenAIMessageContent`] accepts both plain string
//! content and OpenAI's multimodal content-block array, and [`Tool`]
//! accepts both the standard `{"type": "function", "function": {...}}`
//! wrapper and a "flat" shape used by some non-OpenAI clients.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

/// Current Unix timestamp in seconds, used as the default `created` value
/// for models/completions when the caller does not supply one explicitly.
fn unix_timestamp() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs() as i64)
}

/// A single entry in the `/v1/models` listing response.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct OpenAIModel {
    pub id: String,
    #[serde(default = "default_model_object")]
    pub object: String,
    #[serde(default = "unix_timestamp")]
    pub created: i64,
    #[serde(default = "default_owned_by")]
    pub owned_by: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Lanius extension: whether the model returns visible native thinking
    /// (surfaced as `reasoning_content` / Anthropic `thinking` blocks).
    #[serde(default)]
    pub supports_thinking: bool,
}

fn default_model_object() -> String {
    "model".to_owned()
}

fn default_owned_by() -> String {
    "anthropic".to_owned()
}

/// The full body of a `/v1/models` response.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct ModelList {
    #[serde(default = "default_list_object")]
    pub object: String,
    pub data: Vec<OpenAIModel>,
}

fn default_list_object() -> String {
    "list".to_owned()
}

/// A chat message's `content`: a plain string, an array of OpenAI-style
/// multimodal content blocks (e.g. `text`/`image_url`), or (as a
/// compatibility fallback) any other JSON shape.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(untagged)]
pub enum OpenAIMessageContent {
    /// Plain string shorthand content.
    Text(String),
    /// A full array of OpenAI-style multimodal content blocks.
    Blocks(Vec<Value>),
    /// Any other JSON shape, preserved verbatim.
    Other(Value),
}

/// A single message in the `messages` array of a chat completion request.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct ChatMessage {
    /// The message's role (e.g. `"user"`, `"assistant"`, `"tool"`).
    pub role: String,
    /// The message's content, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<OpenAIMessageContent>,
    /// Optional name disambiguating multiple participants with the same
    /// role.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Tool calls requested by the assistant in this turn, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<Value>>,
    /// For `role: "tool"` messages, the id of the tool call this message
    /// answers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

/// The `function` object nested inside a standard-shape [`Tool`].
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct ToolFunction {
    /// The function's name.
    pub name: String,
    /// Human-readable description shown to the model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// JSON Schema describing the function's parameters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameters: Option<Map<String, Value>>,
}

/// A tool definition offered to the model. Supports both the standard
/// OpenAI shape (`kind: "function"` with a nested `function`) and a "flat"
/// shape (`name`/`description`/`input_schema` at the top level, used by
/// some Anthropic-style clients talking to this endpoint).
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct Tool {
    /// Always `"function"` in the standard OpenAI shape.
    #[serde(rename = "type", default = "default_function_type")]
    pub kind: String,
    /// The function definition, in the standard OpenAI shape.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub function: Option<ToolFunction>,
    /// The tool's name, in the "flat" (non-OpenAI) shape.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The tool's description, in the "flat" (non-OpenAI) shape.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The tool's input schema, in the "flat" (non-OpenAI) shape.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_schema: Option<Map<String, Value>>,
}

fn default_function_type() -> String {
    "function".to_owned()
}

/// The `stop` request field: either a single stop sequence or a list of
/// them.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(untagged)]
pub enum StopSequence {
    /// A single stop sequence.
    Single(String),
    /// Multiple stop sequences.
    Multiple(Vec<String>),
}

/// The `tool_choice` request field: either a bare tool name string or an
/// OpenAI-shaped object (e.g. `{"type": "function", "function": {"name":
/// ...}}`).
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(untagged)]
pub enum OpenAIToolChoice {
    /// A bare tool name, forcing that specific tool.
    Name(String),
    /// The full OpenAI-shaped tool-choice object.
    Object(Value),
}

/// The full body of a `/v1/chat/completions` request.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct ChatCompletionRequest {
    /// The target model id.
    pub model: String,
    /// The conversation history.
    pub messages: Vec<ChatMessage>,
    /// Whether to stream the response as Server-Sent Events.
    #[serde(default)]
    pub stream: bool,
    /// Sampling temperature.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    /// Nucleus sampling threshold.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    /// Number of completions to generate (only `1` is supported).
    #[serde(default = "default_n", skip_serializing_if = "Option::is_none")]
    pub n: Option<i64>,
    /// Legacy alias for `max_completion_tokens`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<i64>,
    /// Maximum number of tokens to generate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_completion_tokens: Option<i64>,
    /// Sequence(s) that stop generation when encountered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop: Option<StopSequence>,
    /// Penalizes tokens that have already appeared in the output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub presence_penalty: Option<f64>,
    /// Penalizes tokens by how frequently they have already appeared.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frequency_penalty: Option<f64>,
    /// Requested reasoning effort, mapped onto the target model's supported
    /// levels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<ReasoningEffort>,
    /// Tools offered to the model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<Tool>>,
    /// Controls whether/which tool the model must call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<OpenAIToolChoice>,
    /// Streaming-specific options (e.g. `include_usage`); accepted but not
    /// currently interpreted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_options: Option<Map<String, Value>>,
    /// Per-token logit bias map; accepted but not currently interpreted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logit_bias: Option<BTreeMap<String, f64>>,
    /// Whether to return token log-probabilities; accepted but not
    /// currently interpreted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logprobs: Option<bool>,
    /// Number of top log-probabilities to return per token; accepted but
    /// not currently interpreted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_logprobs: Option<i64>,
    /// Opaque end-user identifier; accepted but not currently interpreted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// Sampling seed for reproducibility; accepted but not currently
    /// interpreted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<i64>,
    /// Whether to allow the model to call multiple tools in parallel;
    /// accepted but not currently interpreted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallel_tool_calls: Option<bool>,
}

fn default_n() -> Option<i64> {
    Some(1)
}

/// The `reasoning_effort` request field, controlling how much internal
/// reasoning the model should use. Mapped onto the target model's supported
/// levels (see [`crate::model::ReasoningCapability::nearest_effort`]);
/// ignored for models without native reasoning.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningEffort {
    /// No reasoning at all.
    None,
    /// Minimal reasoning effort.
    Minimal,
    /// Low reasoning effort.
    Low,
    /// Medium (default) reasoning effort.
    Medium,
    /// High reasoning effort.
    High,
    /// Extra-high reasoning effort.
    Xhigh,
    /// Maximum reasoning effort.
    Max,
}

/// Token usage reported alongside a chat completion (or its final
/// streaming chunk). `credits_used` is a Kiro-specific metering extension,
/// not part of the standard OpenAI schema. Only used by tests below; the
/// live response path computes usage as a `Value` directly in
/// [`super::sse`].
#[cfg(test)]
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
struct ChatCompletionUsage {
    #[serde(default)]
    prompt_tokens: i64,
    #[serde(default)]
    completion_tokens: i64,
    #[serde(default)]
    total_tokens: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    credits_used: Option<f64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn request_accepts_standard_and_flat_tools_with_multimodal_content() {
        let request: ChatCompletionRequest = serde_json::from_value(json!({
            "model": "claude-sonnet-4-5",
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "describe"},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,AA=="}}
            ]}],
            "tools": [
                {"type": "function", "function": {"name": "weather", "parameters": {}}},
                {"name": "cursor_tool", "description": "flat", "input_schema": {}}
            ],
            "reasoning_effort": "xhigh",
            "max_completion_tokens": 42
        }))
        .expect("fixture is valid JSON for the wire model");

        assert!(matches!(
            request.messages[0].content,
            Some(OpenAIMessageContent::Blocks(_))
        ));
        assert_eq!(request.tools.as_ref().map(Vec::len), Some(2));
        assert_eq!(request.reasoning_effort, Some(ReasoningEffort::Xhigh));
    }

    #[test]
    fn optional_response_fields_are_omitted() {
        let usage = ChatCompletionUsage::default();
        assert_eq!(
            serde_json::to_value(usage).expect("serialize usage"),
            json!({
                "prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0
            })
        );
    }

    #[test]
    fn defaults_match_openai_wire_protocol() {
        let model: OpenAIModel = serde_json::from_value(json!({"id": "m"})).expect("model fixture");
        let request: ChatCompletionRequest = serde_json::from_value(json!({
            "model": "m", "messages": [{"role": "developer", "content": "keep role"}]
        }))
        .expect("request fixture");
        assert_eq!(model.object, "model");
        assert_eq!(model.owned_by, "anthropic");
        assert_eq!(request.n, Some(1));
        assert!(!request.stream);
    }
}
