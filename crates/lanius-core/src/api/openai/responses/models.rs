//! Serde wire types for the OpenAI Responses API request body.
//!
//! Only the request side is typed. Input items are kept as raw JSON
//! [`Value`]s because the Responses API has many item kinds (messages,
//! `function_call`, `function_call_output`, `reasoning`, built-in tool
//! records, ...) and [`crate::convert`] only needs a handful of fields from
//! each; unknown item kinds are skipped there rather than rejected here.
//! Response objects and stream events are built as JSON in
//! [`super::sse`].

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::api::ReasoningEffort;

/// The `input` request field: a plain user prompt or a list of input items.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(untagged)]
pub enum ResponsesInput {
    /// Shorthand for a single user message.
    Text(String),
    /// Conversation items (`message`, `function_call`, `function_call_output`, ...).
    Items(Vec<Value>),
}

impl Default for ResponsesInput {
    fn default() -> Self {
        Self::Items(Vec::new())
    }
}

impl ResponsesInput {
    /// Whether the input carries nothing to send.
    ///
    /// # Examples
    ///
    /// ```
    /// use lanius_core::api::ResponsesInput;
    ///
    /// assert!(ResponsesInput::Text(String::new()).is_empty());
    /// assert!(!ResponsesInput::Text("hi".into()).is_empty());
    /// assert!(ResponsesInput::default().is_empty());
    /// ```
    pub fn is_empty(&self) -> bool {
        match self {
            Self::Text(text) => text.is_empty(),
            Self::Items(items) => items.is_empty(),
        }
    }

    /// Converts the input into an item list, rewriting the string shorthand
    /// into an equivalent single user `message` item.
    ///
    /// # Examples
    ///
    /// ```
    /// use lanius_core::api::ResponsesInput;
    ///
    /// let items = ResponsesInput::Text("hi".into()).into_items();
    /// assert_eq!(items[0]["role"], "user");
    /// assert_eq!(items[0]["content"], "hi");
    /// ```
    pub fn into_items(self) -> Vec<Value> {
        match self {
            Self::Text(text) => {
                vec![json!({"type": "message", "role": "user", "content": text})]
            }
            Self::Items(items) => items,
        }
    }
}

/// A tool offered to the model. `function` and `custom` (freeform) tools are
/// forwarded to Kiro; built-in tool types (`web_search`, `file_search`, ...)
/// are accepted and otherwise ignored.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct ResponsesTool {
    /// Tool type, e.g. `"function"` or `"custom"`.
    #[serde(rename = "type")]
    pub kind: String,
    /// Tool name (`function`/`custom` tools).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Human-readable description shown to the model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// JSON Schema of a `function` tool's arguments.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameters: Option<Map<String, Value>>,
    /// Strict schema adherence flag; accepted but not interpreted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strict: Option<bool>,
    /// Input format of a `custom` tool (`text` or a `grammar`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<Value>,
}

/// The `reasoning` request field.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct ResponsesReasoning {
    /// Requested reasoning effort, mapped onto the model's supported levels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<ReasoningEffort>,
    /// Requested summary style; accepted and echoed, native thinking is
    /// always surfaced as a reasoning summary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

/// The body of a `POST /v1/responses` request.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct ResponsesRequest {
    /// The target model id.
    pub model: String,
    /// The conversation input.
    #[serde(default)]
    pub input: ResponsesInput,
    /// System/developer instructions placed ahead of the conversation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    /// Whether to stream the response as Server-Sent Events.
    #[serde(default)]
    pub stream: bool,
    /// Tools offered to the model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ResponsesTool>>,
    /// Tool selection policy; accepted and echoed but not interpreted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<Value>,
    /// Whether parallel tool calls are allowed; accepted and echoed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallel_tool_calls: Option<bool>,
    /// Reasoning configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<ResponsesReasoning>,
    /// Output token cap; accepted and echoed but not interpreted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<i64>,
    /// Sampling temperature; accepted and echoed but not interpreted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    /// Nucleus sampling threshold; accepted and echoed but not interpreted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    /// Id of a stored previous response. Lanius does not store responses,
    /// so requests that set this are rejected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_response_id: Option<String>,
    /// Whether to store the response; accepted but never honored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub store: Option<bool>,
    /// Arbitrary client metadata, echoed back.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Map<String, Value>>,
    /// Text output configuration; accepted and echoed but not interpreted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<Value>,
    /// Context truncation strategy; accepted and echoed but not interpreted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncation: Option<String>,
    /// Opaque end-user identifier, used for API-key session affinity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// Client cache key, used for API-key session affinity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_cache_key: Option<String>,
    /// Extra output to include; accepted but not interpreted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include: Option<Vec<String>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_string_and_item_inputs_with_mixed_tools() {
        let request: ResponsesRequest = serde_json::from_value(json!({
            "model": "m",
            "input": "hello",
            "reasoning": {"effort": "high", "summary": "auto"},
            "tools": [
                {"type": "function", "name": "shell", "parameters": {"type": "object"}},
                {"type": "custom", "name": "apply_patch", "format": {"type": "grammar"}},
                {"type": "web_search", "search_context_size": "low"}
            ]
        }))
        .expect("fixture is a valid request");
        assert_eq!(request.input, ResponsesInput::Text("hello".into()));
        assert_eq!(request.tools.as_ref().map(Vec::len), Some(3));
        assert_eq!(
            request.reasoning.and_then(|reasoning| reasoning.effort),
            Some(ReasoningEffort::High)
        );

        let items: ResponsesRequest = serde_json::from_value(json!({
            "model": "m",
            "input": [{"role": "user", "content": [{"type": "input_text", "text": "hi"}]}]
        }))
        .expect("fixture is a valid request");
        assert!(matches!(items.input, ResponsesInput::Items(ref items) if items.len() == 1));
        assert!(!items.stream);
    }

    #[test]
    fn missing_input_is_empty() {
        let request: ResponsesRequest =
            serde_json::from_value(json!({"model": "m"})).expect("fixture is a valid request");
        assert!(request.input.is_empty());
    }
}
