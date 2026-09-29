//! Token estimation for requests, used when Kiro does not report exact
//! usage numbers.
//!
//! Token counts are estimated using the `cl100k_base` BPE encoding (the same
//! family used by OpenAI models) via [`count_tokens`], with a rough
//! character-based fallback if that encoding is unavailable. Higher-level
//! helpers ([`count_message_tokens`], [`count_tools_tokens`],
//! [`count_system_tokens`], [`estimate_request_tokens`]) walk OpenAI/
//! Anthropic-shaped JSON request bodies (as produced by
//! [`crate::convert`]) to approximate the number of prompt tokens that will
//! be sent upstream. [`calculate_tokens_from_context_usage`] instead derives
//! token counts from Kiro's own `contextUsagePercentage` signal when
//! available, consulting [`crate::model::ModelInfoCache`] for the
//! model's context window size.

use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::model::ModelInfoCache;

/// Empirical multiplier applied to `cl100k_base` token counts to better
/// approximate Claude's own tokenizer, which tends to use more tokens per
/// character than GPT's `cl100k_base` encoding for the same text.
pub const CLAUDE_TOKEN_CORRECTION_FACTOR: f64 = 1.15;

// Lazily-initialized shared BPE encoder; loading it can fail (e.g. missing
// bundled data), in which case every caller transparently falls back to
// `rough_token_estimate` instead of panicking.
static CL100K_BASE: Lazy<Option<tiktoken_rs::CoreBPE>> =
    Lazy::new(|| match tiktoken_rs::cl100k_base() {
        Ok(encoding) => Some(encoding),
        Err(error) => {
            tracing::warn!(%error, "cl100k_base unavailable; using rough token estimate");
            None
        }
    });

/// Breakdown of an estimated prompt token count by source (messages, tools,
/// system prompt), as produced by [`estimate_request_tokens`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestTokenEstimate {
    /// Estimated tokens contributed by the conversation's messages.
    pub messages_tokens: usize,
    /// Estimated tokens contributed by tool/function definitions.
    pub tools_tokens: usize,
    /// Estimated tokens contributed by the system prompt.
    pub system_tokens: usize,
    /// Sum of the three components above.
    pub total_tokens: usize,
}

/// Token usage derived from Kiro's `contextUsagePercentage` signal, as
/// produced by [`calculate_tokens_from_context_usage`]. `prompt_source` and
/// `total_source` document how each figure was obtained, for debugging/UI
/// display.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextUsageTokens {
    /// Estimated prompt (input) tokens.
    pub prompt_tokens: usize,
    /// Estimated completion (output) tokens.
    pub completion_tokens: usize,
    /// Sum of `prompt_tokens` and `completion_tokens`.
    pub total_tokens: usize,
    /// How `prompt_tokens` was derived (e.g. `"context_usage"`, `"unknown"`).
    pub prompt_source: &'static str,
    /// How `total_tokens` was derived.
    pub total_source: &'static str,
}

/// Estimates the number of tokens in a plain text string using
/// `cl100k_base` (falling back to `rough_token_estimate` if unavailable),
/// optionally applying [`CLAUDE_TOKEN_CORRECTION_FACTOR`].
///
/// # Examples
///
/// ```
/// use lanius_core::tokenizer::count_tokens;
///
/// assert_eq!(count_tokens("", false), 0);
/// assert!(count_tokens("hello world", false) > 0);
/// ```
pub fn count_tokens(text: &str, apply_claude_correction: bool) -> usize {
    if text.is_empty() {
        return 0;
    }

    let base_tokens = CL100K_BASE
        .as_ref()
        .map(|encoding| encoding.encode_ordinary(text).len())
        .unwrap_or_else(|| rough_token_estimate(text));
    apply_correction(base_tokens, apply_claude_correction)
}

/// Estimates the total token count contributed by an OpenAI/Anthropic-style
/// `messages` array, including role/content overhead per message and any
/// `tool_calls`/`tool_call_id` fields. Non-object message entries are
/// counted via their scalar text representation rather than skipped, so
/// malformed input never causes a panic or an artificially low estimate.
///
/// # Examples
///
/// ```
/// use lanius_core::tokenizer::count_message_tokens;
/// use serde_json::json;
///
/// let messages = vec![json!({"role": "user", "content": "hello"})];
/// assert!(count_message_tokens(&messages, false) > 0);
/// assert_eq!(count_message_tokens(&[], false), 0);
/// ```
pub fn count_message_tokens(messages: &[Value], apply_claude_correction: bool) -> usize {
    if messages.is_empty() {
        return 0;
    }

    let mut total = 0usize;
    for message in messages {
        let Some(message) = message.as_object() else {
            total = total.saturating_add(4 + count_value_as_text(message));
            continue;
        };

        total = total.saturating_add(4);
        total = total.saturating_add(count_string(message.get("role")));

        if let Some(content) = message.get("content").filter(|value| !value.is_null()) {
            total = total.saturating_add(count_content(content));
        }

        if let Some(tool_calls) = message.get("tool_calls").and_then(Value::as_array) {
            for tool_call in tool_calls {
                total = total.saturating_add(4);
                let function = tool_call.get("function").and_then(Value::as_object);
                total = total.saturating_add(count_string(function.and_then(|f| f.get("name"))));
                total =
                    total.saturating_add(count_string(function.and_then(|f| f.get("arguments"))));
            }
        }

        total = total.saturating_add(count_string(message.get("tool_call_id")));
    }

    apply_correction(total.saturating_add(3), apply_claude_correction)
}

/// Estimates the token count contributed by a `tools` array, accepting both
/// OpenAI's `{"type":"function","function":{...}}` wrapper and Anthropic's
/// flat tool-definition shape, including the JSON-serialized input schema.
///
/// # Examples
///
/// ```
/// use lanius_core::tokenizer::count_tools_tokens;
/// use serde_json::json;
///
/// let tools = vec![json!({"name": "read_file", "description": "Read a file",
///     "input_schema": {"type": "object"}})];
/// assert!(count_tools_tokens(Some(&tools), false) > 0);
/// assert_eq!(count_tools_tokens(None, false), 0);
/// ```
pub fn count_tools_tokens(tools: Option<&[Value]>, apply_claude_correction: bool) -> usize {
    let Some(tools) = tools.filter(|tools| !tools.is_empty()) else {
        return 0;
    };

    let mut total = 0usize;
    for tool in tools {
        total = total.saturating_add(4);
        let payload = if tool.get("type").and_then(Value::as_str) == Some("function") {
            tool.get("function").and_then(Value::as_object)
        } else {
            tool.as_object()
        };

        total = total.saturating_add(count_string(payload.and_then(|p| p.get("name"))));
        total = total.saturating_add(count_string(payload.and_then(|p| p.get("description"))));
        if let Some(schema) =
            payload.and_then(|p| p.get("input_schema").or_else(|| p.get("parameters")))
        {
            total = total.saturating_add(count_tokens(&format_json_spaced(schema), false));
        }
    }

    apply_correction(total, apply_claude_correction)
}

/// Estimates the token count contributed by a system prompt, which may be a
/// plain string or an Anthropic-style array of content blocks (each
/// optionally carrying `cache_control` metadata that is counted too).
///
/// # Examples
///
/// ```
/// use lanius_core::tokenizer::count_system_tokens;
/// use serde_json::json;
///
/// let system = json!("You are a helpful assistant.");
/// assert!(count_system_tokens(Some(&system), false) > 0);
/// assert_eq!(count_system_tokens(None, false), 0);
/// ```
pub fn count_system_tokens(system_prompt: Option<&Value>, apply_claude_correction: bool) -> usize {
    let Some(system_prompt) = system_prompt.filter(|value| !is_empty_value(value)) else {
        return 0;
    };

    let mut total = 0usize;
    match system_prompt {
        Value::String(text) => total = count_tokens(text, false),
        Value::Array(blocks) => {
            for block in blocks {
                if let Some(block) = block.as_object() {
                    total = total.saturating_add(count_string(block.get("text")));
                    if let Some(cache_control) = block.get("cache_control") {
                        total = total.saturating_add(count_tokens(
                            &format_json_spaced(cache_control),
                            false,
                        ));
                    }
                } else {
                    total = total.saturating_add(count_value_as_text(block));
                }
            }
        }
        other => total = count_value_as_text(other),
    }

    apply_correction(total, apply_claude_correction)
}

/// Combines [`count_message_tokens`], [`count_tools_tokens`], and
/// [`count_system_tokens`] into a single [`RequestTokenEstimate`], applying
/// `apply_claude_correction` uniformly across all three components.
///
/// # Examples
///
/// ```
/// use lanius_core::tokenizer::estimate_request_tokens;
/// use serde_json::json;
///
/// let messages = vec![json!({"role": "user", "content": "hello"})];
/// let estimate = estimate_request_tokens(&messages, None, None, false);
/// assert_eq!(estimate.total_tokens, estimate.messages_tokens);
/// ```
pub fn estimate_request_tokens(
    messages: &[Value],
    tools: Option<&[Value]>,
    system_prompt: Option<&Value>,
    apply_claude_correction: bool,
) -> RequestTokenEstimate {
    let messages_tokens = count_message_tokens(messages, apply_claude_correction);
    let tools_tokens = count_tools_tokens(tools, apply_claude_correction);
    let system_tokens = count_system_tokens(system_prompt, apply_claude_correction);
    RequestTokenEstimate {
        messages_tokens,
        tools_tokens,
        system_tokens,
        total_tokens: messages_tokens
            .saturating_add(tools_tokens)
            .saturating_add(system_tokens),
    }
}

/// Derives prompt/total token counts from Kiro's own
/// `contextUsagePercentage` signal (percentage of the model's context window
/// consumed) rather than estimating them locally, since that figure reflects
/// Kiro's actual tokenizer. Falls back to reporting only
/// `completion_tokens` (with `prompt_tokens = 0`) when no usable percentage
/// is available. Note that `prompt_tokens` is derived by subtracting
/// `completion_tokens` from the percentage-derived total, which can
/// under-count the true prompt size if Kiro's percentage already excludes
/// the completion.
///
/// # Examples
///
/// ```
/// use lanius_core::model::ModelInfoCache;
/// use lanius_core::tokenizer::calculate_tokens_from_context_usage;
/// use serde_json::json;
///
/// let cache = ModelInfoCache::default();
/// cache.update(vec![json!({"modelId": "small", "tokenLimits": {"maxInputTokens": 100}})]);
/// let usage = calculate_tokens_from_context_usage(Some(10.0), 5, &cache, "small");
/// assert_eq!(usage.total_tokens, 10);
/// ```
pub fn calculate_tokens_from_context_usage(
    context_usage_percentage: Option<f64>,
    completion_tokens: usize,
    model_cache: &ModelInfoCache,
    model: &str,
) -> ContextUsageTokens {
    if let Some(percentage) = context_usage_percentage.filter(|percentage| *percentage > 0.0) {
        let max_input_tokens = model_cache.get_max_input_tokens(model);
        let total_tokens = ((percentage / 100.0) * f64::from(max_input_tokens)) as usize;
        return ContextUsageTokens {
            prompt_tokens: total_tokens.saturating_sub(completion_tokens),
            completion_tokens,
            total_tokens,
            prompt_source: "subtraction",
            total_source: "API Kiro",
        };
    }

    ContextUsageTokens {
        prompt_tokens: 0,
        completion_tokens,
        total_tokens: completion_tokens,
        prompt_source: "unknown",
        total_source: "tiktoken",
    }
}

fn apply_correction(tokens: usize, apply_claude_correction: bool) -> usize {
    if apply_claude_correction {
        ((tokens as f64) * CLAUDE_TOKEN_CORRECTION_FACTOR) as usize
    } else {
        tokens
    }
}

// Crude fallback estimate (~4 characters per token, plus one) used only
// when the `cl100k_base` encoder failed to load.
fn rough_token_estimate(text: &str) -> usize {
    text.chars().count() / 4 + 1
}

fn count_content(content: &Value) -> usize {
    match content {
        Value::String(text) => count_tokens(text, false),
        Value::Array(items) => items.iter().fold(0usize, |total, item| {
            total.saturating_add(count_content_block(item))
        }),
        _ => 0,
    }
}

// Counts a single Anthropic-style content block by its `type`: text blocks
// count their text; images are approximated with a flat 100-token cost
// (their real token cost depends on model-specific image tiling that isn't
// worth replicating here); tool_use/tool_result blocks count their
// structured fields individually rather than as one opaque JSON blob, so
// that e.g. a large tool result doesn't get double-counted via its
// surrounding envelope.
fn count_content_block(value: &Value) -> usize {
    let Some(item) = value.as_object() else {
        return count_value_as_text(value);
    };
    match item.get("type").and_then(Value::as_str) {
        Some("text") => count_string(item.get("text")),
        Some("image_url") | Some("image") => 100,
        Some("tool_use") => count_string(item.get("id"))
            .saturating_add(count_string(item.get("name")))
            .saturating_add(
                item.get("input")
                    .map(|input| count_tokens(&format_json_spaced(input), false))
                    .unwrap_or(0),
            ),
        Some("tool_result") => {
            let mut total = count_string(item.get("tool_use_id"));
            if let Some(is_error) = item.get("is_error").filter(|value| !value.is_null()) {
                total = total.saturating_add(count_tokens(&display_scalar(is_error), false));
            }
            if let Some(content) = item.get("content").filter(|value| !value.is_null()) {
                total = total.saturating_add(match content {
                    Value::String(text) => count_tokens(text, false),
                    Value::Array(blocks) => blocks.iter().fold(0usize, |subtotal, block| {
                        subtotal.saturating_add(match block.as_object() {
                            Some(block) => match block.get("type").and_then(Value::as_str) {
                                Some("text") => count_string(block.get("text")),
                                Some("image_url") | Some("image") => 100,
                                _ => 0,
                            },
                            None => count_value_as_text(block),
                        })
                    }),
                    other => count_value_as_text(other),
                });
            }
            total
        }
        // Unknown block types are still counted (via their JSON form) rather
        // than silently ignored, so estimates never drop to zero for
        // forward-compatible/unrecognized block shapes.
        _ => count_tokens(&format_json_spaced(value), false),
    }
}

fn count_string(value: Option<&Value>) -> usize {
    value
        .and_then(Value::as_str)
        .map(|value| count_tokens(value, false))
        .unwrap_or(0)
}

fn count_value_as_text(value: &Value) -> usize {
    count_tokens(&display_scalar(value), false)
}

fn is_empty_value(value: &Value) -> bool {
    matches!(value, Value::Null)
        || matches!(value, Value::String(text) if text.is_empty())
        || matches!(value, Value::Array(items) if items.is_empty())
        || matches!(value, Value::Object(items) if items.is_empty())
}

// Re-renders compact `serde_json` output with `", "` after commas and
// `": "` after colons, so token counts based on this text include the
// same separator characters Kiro's own tokenizer would see, without
// pulling in a full custom serializer.
fn format_json_spaced(value: &Value) -> String {
    let compact = value.to_string();
    let mut output = String::with_capacity(compact.len());
    let mut in_string = false;
    let mut escaped = false;
    for character in compact.chars() {
        output.push(character);
        if in_string {
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                in_string = false;
            }
        } else if character == '"' {
            in_string = true;
        } else if character == ',' || character == ':' {
            output.push(' ');
        }
    }
    output
}

// Renders a JSON scalar as plain text for token counting, spelling
// booleans/null as bare words (`None`/`True`/`False`) rather than their
// JSON literal form, matching the established token-count baseline this
// estimator is calibrated against. Used when a value that isn't a plain
// string still needs to be counted as text.
fn display_scalar(value: &Value) -> String {
    match value {
        Value::Null => "None".to_string(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        Value::String(value) => value.clone(),
        Value::Number(value) => value.to_string(),
        Value::Array(_) | Value::Object(_) => format_json_spaced(value),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn correction_and_unicode_fallback_math_are_explicit() {
        assert_eq!(CLAUDE_TOKEN_CORRECTION_FACTOR, 1.15);
        assert_eq!(rough_token_estimate("你好世界"), 2);
        assert_eq!(apply_correction(20, true), 23);
        assert_eq!(count_tokens("", true), 0);
    }

    #[test]
    fn message_tokens_cover_every_supported_content_block() {
        let messages = vec![
            json!({"role":"assistant","content":[
                {"type":"text","text":"hello"},
                {"type":"image","source":{}},
                {"type":"tool_use","id":"toolu_1","name":"weather","input":{"city":"東京"}}
            ]}),
            json!({"role":"user","content":[
                {"type":"tool_result","tool_use_id":"toolu_1","is_error":false,
                 "content":[{"type":"text","text":"sunny"},{"type":"image_url"}]}
            ],"tool_call_id":"call_1"}),
        ];
        assert!(count_message_tokens(&messages, false) > 200);
        assert!(count_message_tokens(&messages, true) > count_message_tokens(&messages, false));
    }

    #[test]
    fn unknown_message_blocks_are_json_counted_and_non_object_messages_do_not_panic() {
        let messages = vec![
            json!({"role":"user","content":[{"type":"unknown","nested":{"x":1}}]}),
            json!(42),
        ];
        assert!(count_message_tokens(&messages, false) > 0);
    }

    #[test]
    fn tool_formats_and_system_formats_are_counted() {
        let schema = json!({"type":"object","properties":{"path":{"type":"string"}}});
        let openai = vec![
            json!({"type":"function","function":{"name":"read","description":"Read a file","parameters":schema}}),
        ];
        let anthropic =
            vec![json!({"name":"read","description":"Read a file","input_schema":schema})];
        assert!(count_tools_tokens(Some(&openai), false) > 4);
        assert!(count_tools_tokens(Some(&anthropic), false) > 4);
        assert_eq!(count_tools_tokens(None, false), 0);

        let system =
            json!([{"type":"text","text":"be helpful","cache_control":{"type":"ephemeral"}}]);
        assert!(
            count_system_tokens(Some(&system), false)
                > count_system_tokens(Some(&json!("be helpful")), false)
        );
    }

    #[test]
    fn request_estimate_is_the_sum_of_its_parts() {
        let messages = vec![json!({"role":"user","content":"hello"})];
        let tools = vec![json!({"name":"x","description":"y","input_schema":{}})];
        let system = json!("rules");
        let estimate = estimate_request_tokens(&messages, Some(&tools), Some(&system), false);
        assert_eq!(
            estimate.total_tokens,
            estimate.messages_tokens + estimate.tools_tokens + estimate.system_tokens
        );
    }

    #[test]
    fn context_usage_uses_cache_and_saturates_prompt_subtraction() {
        let cache = ModelInfoCache::default();
        cache.update(vec![
            json!({"modelId":"small","tokenLimits":{"maxInputTokens":100}}),
        ]);
        let usage = calculate_tokens_from_context_usage(Some(10.0), 20, &cache, "small");
        assert_eq!(usage.total_tokens, 10);
        assert_eq!(usage.prompt_tokens, 0);
        assert_eq!(usage.prompt_source, "subtraction");
        assert_eq!(usage.total_source, "API Kiro");

        let fallback = calculate_tokens_from_context_usage(Some(0.0), 7, &cache, "small");
        assert_eq!(fallback.total_tokens, 7);
        assert_eq!(fallback.prompt_source, "unknown");
    }

    #[test]
    fn spaced_json_preserves_default_separator_tokens() {
        assert_eq!(
            format_json_spaced(&json!({"a":"x:y,z","b":[1,2]})),
            r#"{"a": "x:y,z", "b": [1, 2]}"#
        );
        assert_eq!(display_scalar(&json!(false)), "False");
    }
}
