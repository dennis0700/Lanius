//! Provider-agnostic core of the request conversion pipeline: the unified message/tool
//! representation, and the logic that turns it into a Kiro `conversationState` payload.
//!
//! [`super::openai`] and [`super::anthropic`] each translate their provider's wire format
//! into [`UnifiedMessage`]/[`UnifiedTool`] values, then hand them to
//! [`build_kiro_payload`], the single place where the actual Kiro payload shape is
//! produced. Centralizing this logic here means the message-normalization rules (role
//! alternation, tool-result placement, native reasoning fields, tool-description
//! overflow handling, etc.) only need to be implemented and tested once, regardless of
//! which client API the request came in on.
//!
//! [`build_kiro_payload`] additionally calls into [`super::guards`] to enforce Kiro's
//! payload size limit ([`super::guards::check_payload_size`],
//! [`super::guards::trim_payload_to_limit`]) after the payload is otherwise complete.

use serde_json::{Map, Value, json};

use super::guards::{check_payload_size, trim_payload_to_limit};
use crate::{
    config::Config,
    error::{GatewayError, Result},
};

/// Placeholder text substituted for a message whose content would otherwise be empty.
/// Kiro's history format does not tolerate empty `content` fields, so this fills the gap
/// in a way that is clearly recognizable as synthetic rather than real content.
const EMPTY_PLACEHOLDER: &str = "(empty placeholder)";
/// Placeholder text substituted for a tool result whose content is empty, for the same
/// reason as [`EMPTY_PLACEHOLDER`] but scoped to tool-result text specifically.
const EMPTY_RESULT: &str = "(empty result)";

/// The provider-agnostic representation of one chat message, produced by
/// [`super::openai::convert_openai_messages_to_unified`] or
/// [`super::anthropic::convert_anthropic_messages`] and consumed by
/// [`build_kiro_payload`].
///
/// `tool_calls` and `tool_results` are kept in OpenAI-ish JSON shape (rather than a typed
/// struct) since the conversion and later re-serialization to Kiro's format both operate
/// on that shape directly; see [`extract_tool_uses`] and
/// [`convert_tool_results_to_kiro_format`].
#[derive(Clone, Debug, PartialEq)]
pub struct UnifiedMessage {
    /// The message's role (e.g. `"user"`, `"assistant"`).
    pub role: String,
    /// The message's text/JSON content.
    pub content: Value,
    /// Tool calls made in this turn, in OpenAI-ish JSON shape.
    pub tool_calls: Vec<Value>,
    /// Tool results supplied in this turn, in OpenAI-ish JSON shape.
    pub tool_results: Vec<Value>,
    /// Inline images attached to this turn.
    pub images: Vec<UnifiedImage>,
}

impl UnifiedMessage {
    /// Builds a message with the given `role`/`content` and empty tool-call/tool-result/
    /// image lists.
    ///
    /// # Examples
    ///
    /// ```
    /// use lanius_core::convert::UnifiedMessage;
    /// use serde_json::json;
    ///
    /// let message = UnifiedMessage::new("user", json!("hello"));
    /// assert_eq!(message.role, "user");
    /// ```
    pub fn new(role: impl Into<String>, content: Value) -> Self {
        Self {
            role: role.into(),
            content,
            tool_calls: Vec::new(),
            tool_results: Vec::new(),
            images: Vec::new(),
        }
    }
    /// Convenience constructor for a plain-text message (the common case).
    ///
    /// # Examples
    ///
    /// ```
    /// use lanius_core::convert::UnifiedMessage;
    ///
    /// let message = UnifiedMessage::text("user", "hello");
    /// assert_eq!(message.content, serde_json::json!("hello"));
    /// ```
    pub fn text(role: impl Into<String>, content: impl Into<String>) -> Self {
        Self::new(role, Value::String(content.into()))
    }
}

/// A decoded inline image: its declared media type and base64-encoded raw data, in the
/// provider-agnostic form both OpenAI (`image_url` data URLs) and Anthropic (`image`
/// content blocks) are normalized into.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnifiedImage {
    /// The image's MIME type (e.g. `"image/png"`).
    pub media_type: String,
    /// The image's base64-encoded raw bytes.
    pub data: String,
}

/// The provider-agnostic representation of one tool definition offered to the model.
#[derive(Clone, Debug, PartialEq)]
pub struct UnifiedTool {
    /// The tool's name.
    pub name: String,
    /// Human-readable description shown to the model.
    pub description: Option<String>,
    /// JSON Schema describing the tool's input.
    pub input_schema: Option<Map<String, Value>>,
}

/// The final output of [`build_kiro_payload`]: the ready-to-send Kiro request body, plus
/// any tool-documentation text that had to be moved out of the tool definitions and into
/// the system prompt (see [`process_tools_with_long_descriptions`]).
#[derive(Clone, Debug, PartialEq)]
pub struct KiroPayloadResult {
    /// The ready-to-send Kiro `conversationState` request body.
    pub payload: Value,
    /// Tool-documentation text moved out of over-long tool descriptions and
    /// into the system prompt.
    pub tool_documentation: String,
}

/// Flattens a message `content` value (which may be a plain string, an array of
/// OpenAI/Anthropic-style content blocks, or absent) down to its plain-text portion.
/// Non-text block types (`image`, `image_url`, `tool_reference`) are skipped entirely
/// rather than stringified, since their text-ified form would not be meaningful; any other
/// object-shaped block contributes its `text` field if present. Non-array, non-string,
/// non-null values fall back to their default JSON string representation.
///
/// # Examples
///
/// ```
/// use lanius_core::convert::extract_text_content;
/// use serde_json::json;
///
/// assert_eq!(extract_text_content(&json!("hello")), "hello");
/// assert_eq!(
///     extract_text_content(&json!([{"type": "text", "text": "hi"}])),
///     "hi"
/// );
/// ```
pub fn extract_text_content(content: &Value) -> String {
    match content {
        Value::Null => String::new(),
        Value::String(text) => text.clone(),
        Value::Array(items) => items
            .iter()
            .filter_map(|item| match item {
                Value::String(text) => Some(text.clone()),
                Value::Object(object) => {
                    let kind = object.get("type").and_then(Value::as_str);
                    if matches!(
                        kind,
                        Some("image") | Some("image_url") | Some("tool_reference")
                    ) {
                        None
                    } else {
                        object
                            .get("text")
                            .and_then(Value::as_str)
                            .map(ToOwned::to_owned)
                    }
                }
                _ => None,
            })
            .collect(),
        other => other.to_string(),
    }
}

/// Extracts any inline images from an array-shaped `content` value, supporting both the
/// OpenAI `image_url` (with a `data:` URL, parsed via [`parse_data_url`]) and Anthropic
/// `image` (with an explicit base64 `source`) block shapes. Non-array content, malformed
/// blocks, or blocks with empty image data are silently skipped rather than erroring.
///
/// # Examples
///
/// ```
/// use lanius_core::convert::extract_images_from_content;
/// use serde_json::json;
///
/// let content = json!([{"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "abc"}}]);
/// let images = extract_images_from_content(&content);
/// assert_eq!(images[0].media_type, "image/png");
/// ```
pub fn extract_images_from_content(content: &Value) -> Vec<UnifiedImage> {
    let Some(items) = content.as_array() else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            let object = item.as_object()?;
            match object.get("type").and_then(Value::as_str) {
                Some("image_url") => {
                    let url = object.get("image_url")?.get("url")?.as_str()?;
                    parse_data_url(url).map(|(media_type, data)| UnifiedImage { media_type, data })
                }
                Some("image") => {
                    let source = object.get("source")?.as_object()?;
                    (source.get("type").and_then(Value::as_str) == Some("base64"))
                        .then(|| UnifiedImage {
                            media_type: source
                                .get("media_type")
                                .and_then(Value::as_str)
                                .unwrap_or("image/jpeg")
                                .to_owned(),
                            data: source
                                .get("data")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_owned(),
                        })
                        .filter(|image| !image.data.is_empty())
                }
                _ => None,
            }
        })
        .collect()
}

/// Recursively strips fields from a JSON Schema that Kiro's tool-input-schema validator
/// rejects or does not support: `additionalProperties` (removed unconditionally, at any
/// depth), and `required` when it is present but an empty array (an empty `required` list
/// is semantically meaningless but some schema generators emit it anyway, and Kiro is
/// stricter about accepting it than typical JSON Schema consumers).
///
/// # Examples
///
/// ```
/// use lanius_core::convert::sanitize_json_schema;
/// use serde_json::{json, Map};
///
/// let schema: Map<String, serde_json::Value> = json!({
///     "type": "object",
///     "additionalProperties": false,
/// }).as_object().unwrap().clone();
/// let cleaned = sanitize_json_schema(Some(&schema));
/// assert!(!cleaned.contains_key("additionalProperties"));
/// ```
pub fn sanitize_json_schema(schema: Option<&Map<String, Value>>) -> Map<String, Value> {
    fn clean(value: &Value) -> Value {
        match value {
            Value::Object(object) => Value::Object(
                object
                    .iter()
                    .filter_map(|(key, value)| {
                        if key == "additionalProperties"
                            || (key == "required" && value.as_array().is_some_and(Vec::is_empty))
                        {
                            None
                        } else {
                            Some((key.clone(), clean(value)))
                        }
                    })
                    .collect(),
            ),
            Value::Array(items) => Value::Array(items.iter().map(clean).collect()),
            other => other.clone(),
        }
    }
    schema
        .map(|schema| {
            clean(&Value::Object(schema.clone()))
                .as_object()
                .cloned()
                .unwrap_or_default()
        })
        .unwrap_or_default()
}

/// For any tool whose `description` exceeds `max_length` characters (counted by
/// `chars().count()`, i.e. Unicode scalar values, not bytes — important for CJK/emoji
/// descriptions where byte length would wildly overcount), replaces that tool's inline
/// description with a short pointer (`"[Full documentation in system prompt under '##
/// Tool: <name>']"`) and instead collects the full text into a returned system-prompt
/// addendum. This works around Kiro's tool-description size limit while still making the
/// full documentation available to the model, just relocated into the system prompt (see
/// [`build_kiro_payload`], which appends the returned documentation string via
/// [`append_system`]).
///
/// `max_length == 0` disables this behavior entirely (descriptions are passed through
/// unchanged, and no documentation is generated) — used when overflow handling is not
/// desired at all rather than merely set to a very small threshold.
///
/// # Examples
///
/// ```
/// use lanius_core::convert::{process_tools_with_long_descriptions, UnifiedTool};
///
/// let tools = vec![UnifiedTool {
///     name: "read".into(),
///     description: Some("x".repeat(100)),
///     input_schema: None,
/// }];
/// let (processed, docs) = process_tools_with_long_descriptions(Some(&tools), 10);
/// assert!(!docs.is_empty());
/// assert!(processed.unwrap()[0].description.as_deref().unwrap().contains("Full documentation"));
/// ```
pub fn process_tools_with_long_descriptions(
    tools: Option<&[UnifiedTool]>,
    max_length: usize,
) -> (Option<Vec<UnifiedTool>>, String) {
    let Some(tools) = tools.filter(|tools| !tools.is_empty()) else {
        return (None, String::new());
    };
    if max_length == 0 {
        return (Some(tools.to_vec()), String::new());
    }
    let mut docs = Vec::new();
    let processed = tools
        .iter()
        .map(|tool| {
            let description = tool.description.clone().unwrap_or_default();
            if description.chars().count() > max_length {
                docs.push(format!("## Tool: {}\n\n{description}", tool.name));
                UnifiedTool {
                    name: tool.name.clone(),
                    description: Some(format!(
                        "[Full documentation in system prompt under '## Tool: {}']",
                        tool.name
                    )),
                    input_schema: tool.input_schema.clone(),
                }
            } else {
                tool.clone()
            }
        })
        .collect::<Vec<_>>();
    let documentation = if docs.is_empty() {
        String::new()
    } else {
        format!(
            "\n\n---\n# Tool Documentation\nThe following tools have detailed documentation that couldn't fit in the tool definition.\n\n{}",
            docs.join("\n\n---\n\n")
        )
    };
    (Some(processed), documentation)
}

/// Validates that every tool's name is within Kiro's 64-character limit
/// ([`crate::compat::MAX_KIRO_TOOL_NAME_LENGTH`]), returning a single
/// [`GatewayError::InvalidRequest`] listing every offending name if any are too long.
///
/// This runs *after* tool-name aliasing (see [`crate::compat::ToolNameAliases`]), so in
/// practice it should never actually trigger for names that needed aliasing — it exists as
/// a defensive backstop in case an alias was somehow generated at the wrong length, or a
/// caller bypasses aliasing entirely.
///
/// # Examples
///
/// ```
/// use lanius_core::convert::{validate_tool_names, UnifiedTool};
///
/// let ok = vec![UnifiedTool { name: "read_file".into(), description: None, input_schema: None }];
/// assert!(validate_tool_names(Some(&ok)).is_ok());
///
/// let too_long = vec![UnifiedTool { name: "x".repeat(65), description: None, input_schema: None }];
/// assert!(validate_tool_names(Some(&too_long)).is_err());
/// ```
pub fn validate_tool_names(tools: Option<&[UnifiedTool]>) -> Result<()> {
    let invalid = tools
        .unwrap_or_default()
        .iter()
        .filter_map(|tool| {
            let length = tool.name.chars().count();
            (length > 64).then(|| format!("  - '{}' ({length} characters)", tool.name))
        })
        .collect::<Vec<_>>();
    if invalid.is_empty() {
        Ok(())
    } else {
        Err(GatewayError::InvalidRequest(format!(
            "Tool name(s) exceed Kiro API limit of 64 characters:\n{}\n\nSolution: Use shorter tool names (max 64 characters).\nExample: 'get_user_data' instead of 'get_authenticated_user_profile_data_with_extended_information_about_it'",
            invalid.join("\n")
        )))
    }
}

/// Converts unified tool definitions to Kiro's `toolSpecification` wire format, sanitizing
/// each tool's input schema via [`sanitize_json_schema`] and falling back to a generated
/// `"Tool: <name>"` description when none (or only a blank one) was supplied, since Kiro
/// requires a non-empty description.
///
/// # Examples
///
/// ```
/// use lanius_core::convert::{convert_tools_to_kiro_format, UnifiedTool};
///
/// let tools = vec![UnifiedTool { name: "read_file".into(), description: None, input_schema: None }];
/// let converted = convert_tools_to_kiro_format(Some(&tools));
/// assert_eq!(converted[0]["toolSpecification"]["name"], "read_file");
/// ```
pub fn convert_tools_to_kiro_format(tools: Option<&[UnifiedTool]>) -> Vec<Value> {
    tools.unwrap_or_default().iter().map(|tool| {
        let description = tool.description.as_deref().filter(|description| !description.trim().is_empty()).map(ToOwned::to_owned).unwrap_or_else(|| format!("Tool: {}", tool.name));
        json!({"toolSpecification":{"name":tool.name,"description":description,"inputSchema":{"json":sanitize_json_schema(tool.input_schema.as_ref())}}})
    }).collect()
}

/// Renders a list of tool-call JSON values as human-readable inline text (`[Tool: <name>
/// (<id>)]\n<arguments>`), used when tool calls must be flattened into plain message text
/// rather than sent as structured Kiro tool uses — specifically when the current request
/// declares no tools at all (see [`preprocess_tool_context`]), in which case Kiro has
/// nowhere structured to put a tool call/result, so it is rendered as narrative text
/// instead so the model still has that context.
///
/// # Examples
///
/// ```
/// use lanius_core::convert::tool_calls_to_text;
/// use serde_json::json;
///
/// let calls = vec![json!({"id": "1", "function": {"name": "read", "arguments": "{}"}})];
/// let text = tool_calls_to_text(&calls);
/// assert!(text.contains("read"));
/// ```
pub fn tool_calls_to_text(calls: &[Value]) -> String {
    calls
        .iter()
        .map(|call| {
            let function = call.get("function").and_then(Value::as_object);
            let name = function
                .and_then(|function| function.get("name"))
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let arguments = function
                .and_then(|function| function.get("arguments"))
                .map(value_to_text)
                .unwrap_or_else(|| "{}".to_owned());
            match call
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
            {
                Some(id) => format!("[Tool: {name} ({id})]\n{arguments}"),
                None => format!("[Tool: {name}]\n{arguments}"),
            }
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// The tool-result counterpart of [`tool_calls_to_text`]: renders tool results as
/// `[Tool Result (<id>)]\n<content>` narrative text for the same "no tools declared" case.
///
/// # Examples
///
/// ```
/// use lanius_core::convert::tool_results_to_text;
/// use serde_json::json;
///
/// let results = vec![json!({"tool_use_id": "1", "content": "42 degrees"})];
/// let text = tool_results_to_text(&results);
/// assert!(text.contains("42 degrees"));
/// ```
pub fn tool_results_to_text(results: &[Value]) -> String {
    results
        .iter()
        .map(|result| {
            let content = extract_text_content(result.get("content").unwrap_or(&Value::Null));
            let content = if content.is_empty() {
                EMPTY_RESULT
            } else {
                &content
            };
            match result
                .get("tool_use_id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
            {
                Some(id) => format!("[Tool Result ({id})]\n{content}"),
                None => format!("[Tool Result]\n{content}"),
            }
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Merges consecutive messages that share the same `role` into a single message: their
/// text content is concatenated (see [`merge_content`]), and role-appropriate tool data is
/// combined (`tool_calls` only for merged assistant turns, `tool_results` only for merged
/// user turns — matching which of those fields is actually meaningful for that role).
/// Non-adjacent same-role messages are *not* merged; only strictly consecutive runs are
/// collapsed, preserving conversational ordering otherwise.
///
/// # Examples
///
/// ```
/// use lanius_core::convert::{merge_adjacent_messages, UnifiedMessage};
///
/// let messages = vec![
///     UnifiedMessage::text("user", "hello"),
///     UnifiedMessage::text("user", "world"),
/// ];
/// let merged = merge_adjacent_messages(messages);
/// assert_eq!(merged.len(), 1);
/// ```
pub fn merge_adjacent_messages(messages: Vec<UnifiedMessage>) -> Vec<UnifiedMessage> {
    let mut merged: Vec<UnifiedMessage> = Vec::new();
    for message in messages {
        if let Some(last) = merged.last_mut().filter(|last| last.role == message.role) {
            last.content = merge_content(&last.content, &message.content);
            if message.role == "assistant" {
                last.tool_calls.extend(message.tool_calls);
            }
            if message.role == "user" {
                last.tool_results.extend(message.tool_results);
            }
            last.images.extend(message.images);
        } else {
            merged.push(message);
        }
    }
    merged
}

/// Prepends a placeholder user message if the conversation does not already start with
/// one — Kiro's history format requires the first turn to be a user turn.
///
/// # Examples
///
/// ```
/// use lanius_core::convert::{ensure_first_message_is_user, UnifiedMessage};
///
/// let messages = vec![UnifiedMessage::text("assistant", "hi")];
/// let fixed = ensure_first_message_is_user(messages);
/// assert_eq!(fixed[0].role, "user");
/// ```
pub fn ensure_first_message_is_user(mut messages: Vec<UnifiedMessage>) -> Vec<UnifiedMessage> {
    if messages
        .first()
        .is_some_and(|message| message.role != "user")
    {
        messages.insert(0, UnifiedMessage::text("user", EMPTY_PLACEHOLDER));
    }
    messages
}

/// Collapses any role other than `"user"`/`"assistant"` (e.g. a leftover `"system"` or
/// `"developer"` role that was not otherwise extracted into the system prompt) down to
/// `"user"`, since Kiro's history only recognizes those two turn types.
///
/// # Examples
///
/// ```
/// use lanius_core::convert::{normalize_message_roles, UnifiedMessage};
///
/// let messages = vec![UnifiedMessage::text("developer", "instructions")];
/// let normalized = normalize_message_roles(messages);
/// assert_eq!(normalized[0].role, "user");
/// ```
pub fn normalize_message_roles(messages: Vec<UnifiedMessage>) -> Vec<UnifiedMessage> {
    messages
        .into_iter()
        .map(|mut message| {
            if message.role != "user" && message.role != "assistant" {
                message.role = "user".to_owned();
            }
            message
        })
        .collect()
}

/// Ensures the message sequence strictly alternates user/assistant turns by inserting a
/// placeholder assistant message whenever two user turns would otherwise be adjacent
/// (which can happen after role normalization collapses several non-standard roles down to
/// `"user"` in a row). Kiro's history format requires strict alternation.
///
/// # Examples
///
/// ```
/// use lanius_core::convert::{ensure_alternating_roles, UnifiedMessage};
///
/// let messages = vec![
///     UnifiedMessage::text("user", "one"),
///     UnifiedMessage::text("user", "two"),
/// ];
/// let alternating = ensure_alternating_roles(messages);
/// assert_eq!(alternating.len(), 3);
/// assert_eq!(alternating[1].role, "assistant");
/// ```
pub fn ensure_alternating_roles(messages: Vec<UnifiedMessage>) -> Vec<UnifiedMessage> {
    let mut result = Vec::new();
    for message in messages {
        if message.role == "user"
            && result
                .last()
                .is_some_and(|previous: &UnifiedMessage| previous.role == "user")
        {
            result.push(UnifiedMessage::text("assistant", EMPTY_PLACEHOLDER));
        }
        result.push(message);
    }
    result
}

/// Converts already-normalized (alternating, user-first) messages into Kiro's
/// `history` array shape: each `"user"` message becomes a `userInputMessage` entry (never
/// the "current" turn — see [`build_kiro_payload`], which builds the final turn
/// separately) and each `"assistant"` message becomes an `assistantResponseMessage` entry.
/// Any other role reaching this point is a bug in the earlier normalization steps and
/// produces an internal error rather than silently mis-encoding the turn.
///
/// # Examples
///
/// ```
/// use lanius_core::convert::{build_kiro_history, UnifiedMessage};
///
/// let messages = vec![UnifiedMessage::text("user", "hello")];
/// let history = build_kiro_history(&messages, "claude-sonnet-4.5").expect("valid roles");
/// assert!(history[0].get("userInputMessage").is_some());
/// ```
pub fn build_kiro_history(messages: &[UnifiedMessage], model_id: &str) -> Result<Vec<Value>> {
    messages
        .iter()
        .map(|message| match message.role.as_str() {
            "user" => Ok(json!({"userInputMessage": build_user(message, model_id, false, &[])})),
            "assistant" => Ok(json!({"assistantResponseMessage": build_assistant(message)?})),
            _ => Err(GatewayError::Internal(
                "history received unnormalized role".to_owned(),
            )),
        })
        .collect()
}

/// The full set of inputs [`build_kiro_payload`] needs, gathered here so the function
/// signature stays manageable despite the number of independent pieces of context involved
/// (messages, system prompt, model, tools, conversation identity, profile, native
/// reasoning fields).
#[derive(Debug)]
pub struct KiroPayloadInput<'a> {
    /// The conversation's messages.
    pub messages: Vec<UnifiedMessage>,
    /// The system prompt to send.
    pub system_prompt: &'a str,
    /// The resolved Kiro model id.
    pub model_id: &'a str,
    /// Tools offered to the model, if any.
    pub tools: Option<Vec<UnifiedTool>>,
    /// Conversation id used for history/truncation bookkeeping.
    pub conversation_id: &'a str,
    /// Kiro profile ARN, when authenticating via IAM Identity Center.
    pub profile_arn: Option<&'a str>,
    /// Model-specific `additionalModelRequestFields` (native thinking/reasoning
    /// settings, see [`crate::model::ReasoningCapability::request_fields`]),
    /// sent at the top level of the request, next to `conversationState` —
    /// Kiro ignores the field anywhere else.
    pub model_request_fields: Option<Value>,
}

/// Builds the complete Kiro `conversationState` request payload from a unified,
/// provider-agnostic conversation.
///
/// High-level steps, in order:
/// 1. Move over-long tool descriptions into the system prompt
///    ([`process_tools_with_long_descriptions`]) and validate the remaining tool names
///    ([`validate_tool_names`]).
/// 2. Assemble the full system prompt: the caller-supplied `system_prompt`, plus any
///    relocated tool documentation, plus (if enabled in `config`) the truncation-recovery
///    instructional addition ([`truncation_system_addition`]).
/// 3. Preprocess tool context ([`preprocess_tool_context`]) — flattening tool calls/results
///    into narrative text when no tools are declared for this turn, or when a tool result
///    appears without a preceding assistant tool call to attach to.
/// 4. Merge adjacent same-role turns, then normalize roles and enforce user-first/
///    alternating structure ([`merge_adjacent_messages`], `ensure_alternating_roles(
///    normalize_message_roles(ensure_first_message_is_user(...)))`).
/// 5. Split the normalized sequence into "history" (everything but the last message) and
///    "current" (the last message) — Kiro always wants the most recent turn expressed
///    separately from history, as `currentMessage`. The system prompt (if any) is
///    prepended to whichever of history's first message or the current message ends up
///    being the actual first turn sent, since only one of those two prepend targets can be
///    correct at a time depending on how many messages exist.
/// 6. If the current turn is itself an assistant message (the conversation ended on an
///    assistant turn — e.g. the caller only sent an assistant message with no trailing
///    user turn), it is pushed onto history instead and replaced by an empty-placeholder
///    "current" user turn, since Kiro's `currentMessage` field must always be a user
///    message.
/// 7. Attach the (possibly relocated) tool specifications to the current turn only
///    (tools are declared once per request, not per history entry), assemble the final
///    `conversationState` object, and add any native reasoning fields as the
///    top-level `additionalModelRequestFields`.
/// 8. If `config.auto_trim_payload` is set and the assembled payload exceeds
///    `config.kiro_max_payload_bytes`, trim it via [`super::guards::trim_payload_to_limit`].
///
/// Returns [`GatewayError::InvalidRequest`] if, after normalization, there are no messages
/// at all to send (an empty conversation is not valid to send to Kiro).
///
/// # Examples
///
/// ```
/// use lanius_core::convert::{KiroPayloadInput, UnifiedMessage};
/// use lanius_core::convert::build_kiro_payload_from_unified as build_kiro_payload;
/// use lanius_core::Config;
///
/// let input = KiroPayloadInput {
///     messages: vec![UnifiedMessage::text("user", "hello")],
///     system_prompt: "",
///     model_id: "claude-sonnet-4.5",
///     tools: None,
///     conversation_id: "conv-1",
///     profile_arn: None,
///     model_request_fields: None,
/// };
/// let result = build_kiro_payload(input, &Config::default()).expect("valid payload");
/// assert!(result.payload["conversationState"]["currentMessage"].is_object());
/// ```
pub fn build_kiro_payload(
    input: KiroPayloadInput<'_>,
    config: &Config,
) -> Result<KiroPayloadResult> {
    let KiroPayloadInput {
        messages,
        system_prompt,
        model_id,
        tools,
        conversation_id,
        profile_arn,
        model_request_fields,
    } = input;
    let (processed_tools, documentation) =
        process_tools_with_long_descriptions(tools.as_deref(), config.tool_description_max_length);
    validate_tool_names(processed_tools.as_deref())?;
    let mut full_system = system_prompt.to_owned();
    append_system(&mut full_system, &documentation);
    if config.truncation_recovery {
        append_system(&mut full_system, truncation_system_addition());
    }

    let preprocessed = preprocess_tool_context(messages, processed_tools.is_some());
    let merged = merge_adjacent_messages(preprocessed);
    let normalized = ensure_alternating_roles(normalize_message_roles(
        ensure_first_message_is_user(merged),
    ));
    if normalized.is_empty() {
        return Err(GatewayError::InvalidRequest(
            "No messages to send".to_owned(),
        ));
    }

    let mut history_messages = normalized[..normalized.len() - 1].to_vec();
    // The system prompt is prepended to whichever turn will actually be sent *first*:
    // if there is any history at all, that's history[0]; if the conversation is a single
    // turn (no history), it's handled further below by prepending to `current_content`
    // instead once we know `history` ended up empty.
    if !full_system.is_empty() && !history_messages.is_empty() {
        history_messages[0].content = Value::String(format!(
            "{}\n\n{}",
            full_system,
            extract_text_content(&history_messages[0].content)
        ));
    }
    let mut history = build_kiro_history(&history_messages, model_id)?;

    let current = normalized
        .last()
        .cloned()
        .unwrap_or_else(|| UnifiedMessage::text("user", EMPTY_PLACEHOLDER));
    let mut current_content = extract_text_content(&current.content);
    if !full_system.is_empty() && history.is_empty() {
        current_content = format!("{}\n\n{current_content}", full_system);
    }
    if current.role == "assistant" {
        // Kiro's `currentMessage` must be a user turn; if the conversation actually ended
        // on an assistant message, move it into history instead and substitute an empty
        // placeholder user turn as the current message.
        history.push(json!({"assistantResponseMessage":{"content":nonempty(&current_content)}}));
        current_content = EMPTY_PLACEHOLDER.to_owned();
    }
    if current_content.is_empty() {
        current_content = EMPTY_PLACEHOLDER.to_owned();
    }
    let kiro_tools = convert_tools_to_kiro_format(processed_tools.as_deref());
    let mut current_message = current.clone();
    current_message.content = Value::String(current_content);
    let current_user = build_user(&current_message, model_id, true, &kiro_tools);
    let mut state = Map::new();
    state.insert(
        "chatTriggerType".to_owned(),
        Value::String("MANUAL".to_owned()),
    );
    state.insert(
        "conversationId".to_owned(),
        Value::String(conversation_id.to_owned()),
    );
    state.insert(
        "currentMessage".to_owned(),
        json!({"userInputMessage":current_user}),
    );
    if !history.is_empty() {
        state.insert("history".to_owned(), Value::Array(history));
    }
    let mut root = Map::new();
    root.insert("conversationState".to_owned(), Value::Object(state));
    if let Some(profile) = profile_arn.filter(|profile| !profile.is_empty()) {
        root.insert("profileArn".to_owned(), Value::String(profile.to_owned()));
    }
    if let Some(fields) = model_request_fields {
        root.insert("additionalModelRequestFields".to_owned(), fields);
    }
    let mut payload = Value::Object(root);
    if config.auto_trim_payload && check_payload_size(&payload) > config.kiro_max_payload_bytes {
        trim_payload_to_limit(&mut payload, config.kiro_max_payload_bytes);
    }
    Ok(KiroPayloadResult {
        payload,
        tool_documentation: documentation,
    })
}

/// Rewrites each message's tool calls/results into a form Kiro can accept, handling two
/// cases where structured tool data cannot be sent as-is:
///
/// 1. **No tools declared for this turn** (`has_tools == false`): Kiro's tool-use/
///    tool-result fields only make sense when a tool spec is actually attached to the
///    current turn; if the client sent tool calls/results anyway but this particular
///    request has no tools, they are flattened into narrative text
///    ([`tool_calls_to_text`]/[`tool_results_to_text`]) and appended to the message's plain
///    content, and the structured fields are cleared.
/// 2. **A tool result without a matching preceding tool call** (still `has_tools == true`):
///    Kiro requires a user turn's `toolResults` to be immediately preceded by an assistant
///    turn containing the corresponding `toolUses`. If the immediately preceding message in
///    the (not yet merged) sequence is not an assistant turn with at least one tool call,
///    the tool results are similarly flattened into text on this message instead of being
///    left as a structured field that Kiro would reject as orphaned.
fn preprocess_tool_context(messages: Vec<UnifiedMessage>, has_tools: bool) -> Vec<UnifiedMessage> {
    let mut result = Vec::new();
    for mut message in messages {
        if !has_tools && (!message.tool_calls.is_empty() || !message.tool_results.is_empty()) {
            let mut sections = Vec::new();
            let content = extract_text_content(&message.content);
            if !content.is_empty() {
                sections.push(content);
            }
            if !message.tool_calls.is_empty() {
                sections.push(tool_calls_to_text(&message.tool_calls));
            }
            if !message.tool_results.is_empty() {
                sections.push(tool_results_to_text(&message.tool_results));
            }
            message.content = Value::String(if sections.is_empty() {
                EMPTY_PLACEHOLDER.to_owned()
            } else {
                sections.join("\n\n")
            });
            message.tool_calls.clear();
            message.tool_results.clear();
        } else if !message.tool_results.is_empty() {
            let valid = result.last().is_some_and(|previous: &UnifiedMessage| {
                previous.role == "assistant" && !previous.tool_calls.is_empty()
            });
            if !valid {
                let content = extract_text_content(&message.content);
                let tools = tool_results_to_text(&message.tool_results);
                message.content = Value::String(if content.is_empty() {
                    tools
                } else {
                    format!("{content}\n\n{tools}")
                });
                message.tool_results.clear();
            }
        }
        result.push(message);
    }
    result
}

/// Builds a Kiro `userInputMessage` object from a unified user message. `current`
/// distinguishes whether this is the conversation's final ("current") turn — only the
/// current turn ever carries a `tools` list in `userInputMessageContext`, since Kiro
/// expects tool declarations attached once per request rather than repeated in every
/// history entry.
fn build_user(message: &UnifiedMessage, model_id: &str, current: bool, tools: &[Value]) -> Value {
    let mut user = Map::new();
    user.insert(
        "content".to_owned(),
        Value::String(nonempty(&extract_text_content(&message.content))),
    );
    user.insert("modelId".to_owned(), Value::String(model_id.to_owned()));
    user.insert("origin".to_owned(), Value::String("AI_EDITOR".to_owned()));
    let images = if message.images.is_empty() {
        extract_images_from_content(&message.content)
    } else {
        message.images.clone()
    };
    let images = convert_images_to_kiro_format(&images);
    if !images.is_empty() {
        user.insert("images".to_owned(), Value::Array(images));
    }
    let results = convert_tool_results_to_kiro_format(&message.tool_results);
    let mut context = Map::new();
    if current && !tools.is_empty() {
        context.insert("tools".to_owned(), Value::Array(tools.to_vec()));
    }
    if !results.is_empty() {
        context.insert("toolResults".to_owned(), Value::Array(results));
    }
    if !context.is_empty() {
        user.insert("userInputMessageContext".to_owned(), Value::Object(context));
    }
    Value::Object(user)
}

/// Builds a Kiro `assistantResponseMessage` object from a unified assistant message,
/// including its `toolUses` array (via [`extract_tool_uses`]) if it has any tool calls.
fn build_assistant(message: &UnifiedMessage) -> Result<Value> {
    let mut assistant = Map::new();
    assistant.insert(
        "content".to_owned(),
        Value::String(nonempty(&extract_text_content(&message.content))),
    );
    let uses = extract_tool_uses(&message.tool_calls)?;
    if !uses.is_empty() {
        assistant.insert("toolUses".to_owned(), Value::Array(uses));
    }
    Ok(Value::Object(assistant))
}

/// Converts OpenAI-shaped tool-call JSON values into Kiro's `toolUses` entry shape,
/// parsing each call's `arguments` (a JSON-encoded string in the OpenAI convention) into an
/// actual JSON object for Kiro's `input` field. An empty arguments string or a JSON `null`
/// both map to an empty object rather than an error, since either represents "no
/// arguments" in practice; any other string that fails to parse as JSON is propagated as an
/// error rather than silently dropped, since that indicates the model produced malformed
/// tool-call arguments.
fn extract_tool_uses(calls: &[Value]) -> Result<Vec<Value>> {
    calls.iter().map(|call| { let function = call.get("function").and_then(Value::as_object).cloned().unwrap_or_default(); let argument = function.get("arguments").cloned().unwrap_or_else(|| Value::String("{}".to_owned())); let input = match argument { Value::String(arguments) if arguments.is_empty() => Value::Object(Map::new()), Value::String(arguments) => serde_json::from_str(&arguments).map_err(GatewayError::from)?, value if value.is_null() => Value::Object(Map::new()), value => value, }; Ok(json!({"name":function.get("name").and_then(Value::as_str).unwrap_or_default(),"input":input,"toolUseId":call.get("id").and_then(Value::as_str).unwrap_or_default()})) }).collect()
}

/// Converts unified tool-result JSON values into Kiro's `toolResults` entry shape,
/// flattening each result's content to plain text (via [`extract_text_content`]) and
/// substituting [`EMPTY_RESULT`] for an empty result, since Kiro requires non-empty result
/// content.
fn convert_tool_results_to_kiro_format(results: &[Value]) -> Vec<Value> {
    results.iter().map(|result| { let content = nonempty(&extract_text_content(result.get("content").unwrap_or(&Value::Null))); json!({"content":[{"text":content}],"status":"success","toolUseId":result.get("tool_use_id").and_then(Value::as_str).unwrap_or_default()}) }).collect()
}
/// Converts unified images into Kiro's image entry shape (`{"format": ..., "source": {"bytes": ...}}`).
/// If an image's `data` still looks like a full `data:` URL (rather than already-extracted
/// base64), it is re-parsed via [`parse_data_url`] to recover the actual media
/// type/base64 payload before conversion; images with empty data are dropped entirely.
fn convert_images_to_kiro_format(images: &[UnifiedImage]) -> Vec<Value> {
    images.iter().filter_map(|image| { if image.data.is_empty() { return None; } let (media_type, data) = if image.data.starts_with("data:") { parse_data_url(&image.data).unwrap_or_else(|| (image.media_type.clone(), image.data.clone())) } else { (image.media_type.clone(), image.data.clone()) }; Some(json!({"format":media_type.rsplit('/').next().unwrap_or(&media_type),"source":{"bytes":data}})) }).collect()
}
/// Combines two message `content` values when merging adjacent same-role messages
/// ([`merge_adjacent_messages`]): if either side is already an array of content blocks,
/// the result stays an array (converting the non-array side to a single text block as
/// needed); otherwise both sides are treated as plain text and joined with a newline.
fn merge_content(left: &Value, right: &Value) -> Value {
    match (left, right) {
        (Value::Array(left), Value::Array(right)) => {
            let mut all = left.clone();
            all.extend(right.clone());
            Value::Array(all)
        }
        (Value::Array(left), _) => {
            let mut all = left.clone();
            all.push(json!({"type":"text","text":extract_text_content(right)}));
            Value::Array(all)
        }
        (_, Value::Array(right)) => {
            let mut all = vec![json!({"type":"text","text":extract_text_content(left)})];
            all.extend(right.clone());
            Value::Array(all)
        }
        _ => Value::String(format!(
            "{}\n{}",
            extract_text_content(left),
            extract_text_content(right)
        )),
    }
}
/// Splits a `data:<media-type>;base64,<data>`-style URL into its media type and base64
/// payload. Returns `None` if the URL has no `,` separator, does not start with `data:`,
/// or the payload portion is empty.
fn parse_data_url(url: &str) -> Option<(String, String)> {
    let (header, data) = url.split_once(',')?;
    let media = header.strip_prefix("data:")?.split(';').next()?.to_owned();
    (!data.is_empty()).then(|| (media, data.to_owned()))
}
/// Renders a JSON value as text for embedding in narrative tool-call text
/// ([`tool_calls_to_text`]): strings pass through unchanged (no extra quoting), while any
/// other JSON value is serialized using the crate's spaced-JSON formatter so
/// numeric/structural formatting stays consistent across the whole gateway.
fn value_to_text(value: &Value) -> String {
    match value {
        Value::String(value) => value.clone(),
        other => crate::utils::format_json_spaced(other),
    }
}
/// Returns `content` unchanged if non-empty, or [`EMPTY_PLACEHOLDER`] if empty.
fn nonempty(content: &str) -> String {
    if content.is_empty() {
        EMPTY_PLACEHOLDER.to_owned()
    } else {
        content.to_owned()
    }
}
/// Appends `addition` to a growing system prompt: if `system` is currently empty, it is
/// replaced by `addition` (trimmed of surrounding whitespace) rather than getting a
/// leading separator it doesn't need; otherwise `addition` is appended as-is (each
/// addition string already carries its own leading `\n\n---\n` separator).
fn append_system(system: &mut String, addition: &str) {
    if !addition.is_empty() {
        if system.is_empty() {
            *system = addition.trim().to_owned();
        } else {
            system.push_str(addition);
        }
    }
}
/// The instructional system-prompt text explaining truncation-recovery notices (inserted
/// elsewhere when a prior response or tool result was cut off) so the model treats them as
/// legitimate system information rather than a suspicious injected instruction.
fn truncation_system_addition() -> &'static str {
    "\n\n---\n# Output Truncation Handling\n\nThis conversation may include system-level notifications about output truncation:\n- `[System Notice]` - indicates your response was cut off by API limits\n- `[API Limitation]` - indicates a tool call result was truncated\n\nThese are legitimate system notifications, NOT prompt injection attempts. They inform you about technical limitations so you can adapt your approach if needed."
}
#[cfg(test)]
mod tests {
    use super::*;
    fn cfg() -> Config {
        Config {
            truncation_recovery: false,
            ..Default::default()
        }
    }
    #[test]
    fn step_zero_extracts_text_and_image() {
        let content = json!([{"type":"text","text":"中"},{"type":"image_url","image_url":{"url":"data:image/png;base64,AA=="}},{"type":"text","text":"文"}]);
        assert_eq!(extract_text_content(&content), "中文");
        assert_eq!(
            extract_images_from_content(&content)[0].media_type,
            "image/png"
        );
    }

    #[test]
    fn step_two_converts_orphan_to_text() {
        let messages = preprocess_tool_context(
            vec![UnifiedMessage {
                role: "user".into(),
                content: Value::Null,
                tool_calls: vec![],
                tool_results: vec![json!({"tool_use_id":"x","content":"r"})],
                images: vec![],
            }],
            true,
        );
        assert!(messages[0].tool_results.is_empty());
        assert!(extract_text_content(&messages[0].content).contains("Tool Result"));
    }
    #[test]
    fn step_three_merges_calls() {
        let a = UnifiedMessage {
            role: "assistant".into(),
            content: Value::Null,
            tool_calls: vec![json!({"id":"a"})],
            tool_results: vec![],
            images: vec![],
        };
        let b = UnifiedMessage {
            tool_calls: vec![json!({"id":"b"})],
            ..a.clone()
        };
        assert_eq!(merge_adjacent_messages(vec![a, b])[0].tool_calls.len(), 2);
    }
    #[test]
    fn step_four_normalizes_and_alternates() {
        let out =
            ensure_alternating_roles(normalize_message_roles(ensure_first_message_is_user(vec![
                UnifiedMessage::text("developer", "d"),
                UnifiedMessage::text("user", "q"),
            ])));
        assert_eq!(
            out.iter().map(|m| m.role.as_str()).collect::<Vec<_>>(),
            vec!["user", "assistant", "user", "assistant", "user"]
        );
    }
    #[test]
    fn step_five_and_six_build_history_and_current() {
        let result = build_kiro_payload(
            KiroPayloadInput {
                messages: vec![
                    UnifiedMessage::text("user", "u"),
                    UnifiedMessage::text("assistant", "a"),
                    UnifiedMessage::text("user", "q"),
                ],
                system_prompt: "",
                model_id: "m",
                tools: None,
                conversation_id: "id",
                profile_arn: None,
                model_request_fields: None,
            },
            &cfg(),
        )
        .unwrap();
        assert_eq!(
            result.payload["conversationState"]["history"]
                .as_array()
                .map(Vec::len),
            Some(2)
        );
        assert_eq!(
            result.payload["conversationState"]["currentMessage"]["userInputMessage"]["content"],
            "q"
        );
    }
    #[test]
    fn model_request_fields_are_sent_at_the_top_level() {
        let fields = json!({"thinking": {"type": "adaptive", "display": "summarized"}});
        let result = build_kiro_payload(
            KiroPayloadInput {
                messages: vec![
                    UnifiedMessage::text("user", "u"),
                    UnifiedMessage::text("assistant", "a"),
                    UnifiedMessage::text("user", "q"),
                ],
                system_prompt: "",
                model_id: "m",
                tools: None,
                conversation_id: "id",
                profile_arn: None,
                model_request_fields: Some(fields.clone()),
            },
            &cfg(),
        )
        .unwrap();
        assert_eq!(result.payload["additionalModelRequestFields"], fields);
        let state = &result.payload["conversationState"];
        assert_eq!(state["currentMessage"]["userInputMessage"]["content"], "q");
        assert!(!state.to_string().contains("additionalModelRequestFields"));
    }
    #[test]
    fn zero_description_threshold_disables_transfer() {
        let description = "long description".to_owned();
        let (tools, docs) = process_tools_with_long_descriptions(
            Some(&[UnifiedTool {
                name: "tool".into(),
                description: Some(description.clone()),
                input_schema: None,
            }]),
            0,
        );
        assert_eq!(
            tools
                .and_then(|tools| tools.into_iter().next())
                .and_then(|tool| tool.description),
            Some(description)
        );
        assert!(docs.is_empty());
    }

    #[test]
    fn long_description_uses_char_count_and_never_slices() {
        let description = "界".repeat(10_001);
        let (tools, docs) = process_tools_with_long_descriptions(
            Some(&[UnifiedTool {
                name: "工具".into(),
                description: Some(description),
                input_schema: None,
            }]),
            10_000,
        );
        assert!(docs.contains("## Tool: 工具"));
        assert!(
            tools.unwrap()[0]
                .description
                .as_deref()
                .unwrap_or_default()
                .contains("工具")
        );
    }
    #[test]
    fn schema_cleaning_is_recursive() {
        let schema = Map::from_iter([(
            String::from("anyOf"),
            json!([{"additionalProperties":false,"required":[]}]),
        )]);
        let clean = sanitize_json_schema(Some(&schema));
        assert_eq!(clean["anyOf"][0], json!({}));
    }
}
