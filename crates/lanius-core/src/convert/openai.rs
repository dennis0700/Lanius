//! Converts an OpenAI-shaped chat completion request into a Kiro request payload.
//!
//! This is the OpenAI-specific half of the [`super`] conversion pipeline: it translates
//! `crate::api::openai::models` types into [`super::core::UnifiedMessage`]/
//! [`super::core::UnifiedTool`], then delegates to [`super::core::build_kiro_payload`] for
//! the provider-agnostic normalization and payload assembly. See [`super::anthropic`] for
//! the Anthropic-specific equivalent, and [`super::core`] for the shared logic both call
//! into.

use serde_json::Value;
use std::collections::HashMap;

use super::core::{self, UnifiedMessage, UnifiedTool};
use crate::api::openai::models::{
    ChatCompletionRequest, ChatMessage, OpenAIMessageContent, ReasoningEffort, Tool,
};
use crate::compat::ToolNameAliases;
use crate::config::Config;
use crate::error::Result;
use crate::model::{EffortLevel, ModelInfoCache, ReasoningRequest, get_model_id_for_kiro};

/// Converts an OpenAI message list into a `(system_prompt, unified_messages)` pair.
///
/// System messages are extracted out of the message list entirely and joined with `\n`
/// into a single system prompt string, rather than being represented as a unified message
/// — OpenAI allows multiple `system` messages anywhere in the list, but Kiro (via
/// [`super::core::build_kiro_payload`]) wants one combined system prompt up front.
///
/// Tool results (`role: "tool"` messages) are buffered rather than emitted immediately: OpenAI
/// represents each tool result as its own separate message, but Kiro/the unified
/// representation expects tool results to be attached to the *next* user turn's
/// `tool_results` field. [`flush_tool_results`] is called both whenever a non-tool message
/// is encountered (attaching any buffered results, plus any images collected alongside
/// them, to a synthetic user message before that point) and once more at the end, so
/// trailing tool results are not silently dropped if the message list ends with them.
///
/// # Examples
///
/// ```
/// use lanius_core::api::ChatMessage;
/// use lanius_core::compat::ToolNameAliases;
/// use lanius_core::convert::convert_openai_messages_to_unified;
///
/// let messages = vec![ChatMessage {
///     role: "user".into(),
///     content: Some(lanius_core::api::OpenAIMessageContent::Text("hello".into())),
///     name: None,
///     tool_calls: None,
///     tool_call_id: None,
/// }];
/// let (system, unified) = convert_openai_messages_to_unified(&messages, &mut ToolNameAliases::default())
///     .expect("conversion succeeds");
/// assert_eq!(system, "");
/// assert_eq!(unified[0].role, "user");
/// ```
pub fn convert_openai_messages_to_unified(
    messages: &[ChatMessage],
    aliases: &mut ToolNameAliases,
) -> Result<(String, Vec<UnifiedMessage>)> {
    let mut system_parts = Vec::new();
    let mut non_system = Vec::new();
    for message in messages {
        if message.role == "system" {
            system_parts.push(core::extract_text_content(&content_value(
                message.content.as_ref(),
            )?));
        } else {
            non_system.push(message);
        }
    }
    let mut result = Vec::new();
    let mut pending_results = Vec::new();
    let mut pending_images = Vec::new();
    for message in non_system {
        let content = content_value(message.content.as_ref())?;
        if message.role == "tool" {
            pending_results.push(serde_json::json!({"type":"tool_result","tool_use_id":message.tool_call_id.as_deref().unwrap_or_default(),"content":nonempty_result(core::extract_text_content(&content))}));
            pending_images.extend(core::extract_images_from_content(&content));
            continue;
        }
        flush_tool_results(&mut result, &mut pending_results, &mut pending_images);
        let mut unified = UnifiedMessage::new(
            message.role.clone(),
            Value::String(core::extract_text_content(&content)),
        );
        if message.role == "assistant" {
            // Tool-call names are aliased here (outbound direction) so the rest of the
            // pipeline, including Kiro's tool-use validation, only ever sees Kiro-legal
            // names.
            unified.tool_calls = message
                .tool_calls
                .clone()
                .unwrap_or_default()
                .into_iter()
                .filter(Value::is_object)
                .map(|call| normalize_openai_call(call, aliases))
                .collect();
        }
        if message.role == "user" {
            // A user message's content can itself embed `tool_result`-shaped blocks (some
            // clients inline tool results into a user turn rather than using a separate
            // `role: "tool"` message); those are extracted here too.
            unified.tool_results = extract_embedded_tool_results(&content);
            unified.images = core::extract_images_from_content(&content);
        }
        result.push(unified);
    }
    flush_tool_results(&mut result, &mut pending_results, &mut pending_images);
    Ok((system_parts.join("\n").trim().to_owned(), result))
}

/// Converts OpenAI `tools` (function-type only; any other `type` is skipped) into unified
/// tool definitions, aliasing each tool's name via `aliases.alias_for` so it is Kiro-legal
/// regardless of the original name's length/characters. Supports both the standard
/// `{"type":"function","function":{...}}` shape and a flatter `{"name":...}` shape some
/// clients send directly on the tool object. Returns `None` if there are no usable tool
/// entries at all (as opposed to `Some(vec![])`), matching the "tools were not offered"
/// semantics [`super::core::build_kiro_payload`] expects.
///
/// # Examples
///
/// ```
/// use lanius_core::api::{Tool, ToolFunction};
/// use lanius_core::compat::ToolNameAliases;
/// use lanius_core::convert::convert_openai_tools_to_unified;
///
/// let tools = vec![Tool {
///     kind: "function".into(),
///     function: Some(ToolFunction { name: "read_file".into(), description: None, parameters: None }),
///     name: None,
///     description: None,
///     input_schema: None,
/// }];
/// let unified = convert_openai_tools_to_unified(Some(&tools), &mut ToolNameAliases::default());
/// assert_eq!(unified.unwrap()[0].name, "read_file");
/// ```
pub fn convert_openai_tools_to_unified(
    tools: Option<&[Tool]>,
    aliases: &mut ToolNameAliases,
) -> Option<Vec<UnifiedTool>> {
    let converted = tools
        .unwrap_or_default()
        .iter()
        .filter_map(|tool| {
            if tool.kind != "function" {
                return None;
            }
            if let Some(function) = &tool.function {
                Some(UnifiedTool {
                    name: aliases.alias_for(&function.name),
                    description: function.description.clone(),
                    input_schema: function.parameters.clone(),
                })
            } else {
                tool.name.as_ref().map(|name| UnifiedTool {
                    name: aliases.alias_for(name),
                    description: tool.description.clone(),
                    input_schema: tool.input_schema.clone(),
                })
            }
        })
        .collect::<Vec<_>>();
    (!converted.is_empty()).then_some(converted)
}

/// Normalizes an OpenAI request's `reasoning_effort` into a [`ReasoningRequest`]:
/// `none` disables reasoning, any other level becomes the requested effort (later
/// snapped to the model's supported levels), and an absent field leaves the model
/// default in place.
///
/// # Examples
///
/// ```
/// use lanius_core::api::ChatCompletionRequest;
/// use lanius_core::convert::reasoning_request_from_openai;
/// use lanius_core::model::EffortLevel;
/// use serde_json::json;
///
/// let request: ChatCompletionRequest = serde_json::from_value(json!({
///     "model": "m", "messages": [], "reasoning_effort": "minimal"
/// })).expect("valid request");
/// assert_eq!(reasoning_request_from_openai(&request).effort, Some(EffortLevel::Minimal));
/// ```
pub fn reasoning_request_from_openai(request: &ChatCompletionRequest) -> ReasoningRequest {
    let effort = request.reasoning_effort.map(|effort| match effort {
        ReasoningEffort::None => EffortLevel::None,
        ReasoningEffort::Minimal => EffortLevel::Minimal,
        ReasoningEffort::Low => EffortLevel::Low,
        ReasoningEffort::Medium => EffortLevel::Medium,
        ReasoningEffort::High => EffortLevel::High,
        ReasoningEffort::Xhigh => EffortLevel::Xhigh,
        ReasoningEffort::Max => EffortLevel::Max,
    });
    ReasoningRequest {
        disabled: effort == Some(EffortLevel::None),
        effort: effort.filter(|level| *level != EffortLevel::None),
        display: None,
    }
}

/// Top-level entry point: converts a full OpenAI [`ChatCompletionRequest`] into a Kiro
/// request payload.
///
/// Tool names are registered up front via [`register_openai_tool_names`] before message
/// conversion runs, so that any tool-call references inside the message history are
/// aliased consistently with the top-level tool declarations (registering first avoids a
/// name being aliased one way in a message and a different way in the tools array, which
/// could happen if collision-avoidance decisions were made independently in each pass).
///
/// Native reasoning fields are attached only when `model_cache` reports the target
/// model supports them (see [`crate::model::ReasoningCapability`]); other models get
/// no reasoning fields and produce no thinking output.
///
/// # Examples
///
/// ```
/// use lanius_core::api::ChatCompletionRequest;
/// use lanius_core::compat::ToolNameAliases;
/// use lanius_core::convert::build_kiro_payload;
/// use lanius_core::model::ModelInfoCache;
/// use lanius_core::Config;
/// use serde_json::json;
///
/// let request: ChatCompletionRequest = serde_json::from_value(json!({
///     "model": "claude-sonnet-4-5", "messages": [{"role": "user", "content": "hello"}]
/// })).expect("valid request");
/// let mut aliases = ToolNameAliases::default();
/// let cache = ModelInfoCache::default();
/// let result = build_kiro_payload(&request, "conv-1", None, &Config::default(), &cache, &mut aliases)
///     .expect("conversion succeeds");
/// assert!(result.payload["conversationState"]["currentMessage"].is_object());
/// ```
pub fn build_kiro_payload(
    request: &ChatCompletionRequest,
    conversation_id: &str,
    profile_arn: Option<&str>,
    config: &Config,
    model_cache: &ModelInfoCache,
    aliases: &mut ToolNameAliases,
) -> Result<core::KiroPayloadResult> {
    register_openai_tool_names(request.tools.as_deref(), aliases);
    let (system, messages) = convert_openai_messages_to_unified(&request.messages, aliases)?;
    let tools = convert_openai_tools_to_unified(request.tools.as_deref(), aliases);
    let model = get_model_id_for_kiro(&request.model, &HashMap::new());
    let model_request_fields = model_cache
        .reasoning_capability(&model)
        .and_then(|capability| capability.request_fields(&reasoning_request_from_openai(request)));
    core::build_kiro_payload(
        core::KiroPayloadInput {
            messages,
            system_prompt: &system,
            model_id: &model,
            tools,
            conversation_id,
            profile_arn,
            model_request_fields,
        },
        config,
    )
}

/// Registers every function tool's name with `aliases` before any individual message or
/// tool conversion happens, per [`build_kiro_payload`]'s ordering rationale.
fn register_openai_tool_names(tools: Option<&[Tool]>, aliases: &mut ToolNameAliases) {
    aliases.register_names(tools.unwrap_or_default().iter().filter_map(|tool| {
        (tool.kind == "function")
            .then(|| {
                tool.function
                    .as_ref()
                    .map(|function| function.name.as_str())
                    .or(tool.name.as_deref())
            })
            .flatten()
    }));
}

/// Normalizes an [`OpenAIMessageContent`] (which may be absent, a plain string, an array
/// of content blocks, or an arbitrary JSON value from a non-conforming client) into a
/// single [`Value`] for downstream text/image extraction.
fn content_value(content: Option<&OpenAIMessageContent>) -> Result<Value> {
    match content {
        None => Ok(Value::Null),
        Some(OpenAIMessageContent::Text(text)) => Ok(Value::String(text.clone())),
        Some(OpenAIMessageContent::Blocks(blocks)) => Ok(Value::Array(blocks.clone())),
        Some(OpenAIMessageContent::Other(value)) => Ok(value.clone()),
    }
}
/// Rebuilds an OpenAI tool-call JSON value with its function name replaced by the
/// corresponding alias, preserving the call `id` and raw `arguments` string (defaulting
/// arguments to `"{}"` if absent) unchanged.
fn normalize_openai_call(call: Value, aliases: &mut ToolNameAliases) -> Value {
    let object = call.as_object();
    let function = object
        .and_then(|object| object.get("function"))
        .and_then(Value::as_object);
    serde_json::json!({"id":object.and_then(|object| object.get("id")).and_then(Value::as_str).unwrap_or_default(),"type":"function","function":{"name":function.and_then(|function| function.get("name")).and_then(Value::as_str).map(|name| aliases.alias_for(name)).unwrap_or_default(),"arguments":function.and_then(|function| function.get("arguments")).cloned().unwrap_or_else(|| Value::String("{}".to_owned()))}})
}
/// Extracts `tool_result`-typed content blocks embedded directly inside a user message's
/// content array (as opposed to a separate `role: "tool"` message), converting each into
/// the same unified tool-result JSON shape used elsewhere.
fn extract_embedded_tool_results(content: &Value) -> Vec<Value> {
    content
        .as_array()
        .map_or(&[] as &[Value], Vec::as_slice)
        .iter().filter_map(|item| { let object=item.as_object()?; (object.get("type").and_then(Value::as_str)==Some("tool_result")).then(|| serde_json::json!({"type":"tool_result","tool_use_id":object.get("tool_use_id").and_then(Value::as_str).unwrap_or_default(),"content":nonempty_result(core::extract_text_content(object.get("content").unwrap_or(&Value::Null)))})) }).collect()
}
/// If any tool results have been buffered, wraps them (and any images collected alongside
/// them) into a synthetic empty-content user message and appends it to `output`, then
/// clears both buffers. A no-op if no results are buffered, so calling this between every
/// message is safe and only produces a new message when there is actually something to
/// flush.
fn flush_tool_results(
    output: &mut Vec<UnifiedMessage>,
    results: &mut Vec<Value>,
    images: &mut Vec<core::UnifiedImage>,
) {
    if !results.is_empty() {
        let mut message = UnifiedMessage::text("user", "");
        message.tool_results = std::mem::take(results);
        message.images = std::mem::take(images);
        output.push(message);
    }
}
/// Returns `value` unchanged if non-empty, or the fixed `"(empty result)"` placeholder
/// otherwise, matching [`super::core`]'s `EMPTY_RESULT` convention for tool results that
/// are extracted at this earlier, provider-specific stage rather than later in `core`.
fn nonempty_result(value: String) -> String {
    if value.is_empty() {
        "(empty result)".to_owned()
    } else {
        value
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::openai::models::{ChatCompletionRequest, ChatMessage};
    use serde_json::json;
    #[test]
    fn extracts_system_and_groups_tool_results() {
        let messages = vec![
            ChatMessage {
                role: "system".into(),
                content: Some(OpenAIMessageContent::Text("S".into())),
                name: None,
                tool_calls: None,
                tool_call_id: None,
            },
            ChatMessage {
                role: "tool".into(),
                content: Some(OpenAIMessageContent::Text("r".into())),
                name: None,
                tool_calls: None,
                tool_call_id: Some("c".into()),
            },
        ];
        let (system, output) =
            convert_openai_messages_to_unified(&messages, &mut ToolNameAliases::default()).unwrap();
        assert_eq!(system, "S");
        assert_eq!(output.len(), 1);
        assert_eq!(output[0].tool_results[0]["tool_use_id"], "c");
    }
    #[test]
    fn data_url_image_and_multibyte_content_do_not_panic() {
        let message = ChatMessage {
            role: "user".into(),
            content: Some(OpenAIMessageContent::Blocks(vec![
                json!({"type":"text","text":"中文"}),
                json!({"type":"image_url","image_url":{"url":"data:image/png;base64,AA=="}}),
            ])),
            name: None,
            tool_calls: None,
            tool_call_id: None,
        };
        let (_, out) =
            convert_openai_messages_to_unified(&[message], &mut ToolNameAliases::default())
                .unwrap();
        assert_eq!(out[0].images.len(), 1);
        assert_eq!(core::extract_text_content(&out[0].content), "中文");
    }
    #[test]
    fn aliases_reserve_legal_tool_names_before_history_conversion() {
        let invalid = "has spaces";
        let legal_collision = ToolNameAliases::default().alias_for(invalid);
        let request: ChatCompletionRequest = serde_json::from_value(json!({
            "model": "m",
            "messages": [
                {"role": "assistant", "content": "calling", "tool_calls": [{"id": "call", "function": {"name": legal_collision, "arguments": "{}"}}]},
                {"role": "user", "content": "continue"}
            ],
            "tools": [
                {"type": "function", "function": {"name": invalid, "parameters": {}}},
                {"type": "function", "function": {"name": legal_collision, "parameters": {}}}
            ]
        }))
        .unwrap_or_else(|error| panic!("alias collision fixture must deserialize: {error}"));
        let config = Config {
            truncation_recovery: false,
            ..Default::default()
        };
        let mut aliases = ToolNameAliases::default();
        let payload = build_kiro_payload(
            &request,
            "id",
            None,
            &config,
            &ModelInfoCache::default(),
            &mut aliases,
        )
        .unwrap_or_else(|error| panic!("conversion failed: {error}"))
        .payload;
        let names = payload["conversationState"]["currentMessage"]["userInputMessage"]
            ["userInputMessageContext"]["tools"]
            .as_array()
            .unwrap_or_else(|| panic!("tool specifications must be present"))
            .iter()
            .filter_map(|tool| tool["toolSpecification"]["name"].as_str())
            .collect::<Vec<_>>();
        assert_eq!(names.len(), 2);
        assert_ne!(names[0], names[1]);
        assert!(names.contains(&legal_collision.as_str()));
        assert_eq!(
            payload["conversationState"]["history"][1]["assistantResponseMessage"]["toolUses"][0]["name"],
            legal_collision
        );
    }

    #[test]
    fn reasoning_fields_follow_the_model_schema() {
        let cache = ModelInfoCache::default();
        cache.update(vec![
            json!({"modelId": "claude-opus-5.5", "additionalModelRequestFieldsSchema": {"properties": {
                "thinking": {"properties": {
                    "type": {"enum": ["adaptive"]},
                    "display": {"enum": ["summarized", "omitted"]}
                }},
                "output_config": {"properties": {"effort": {"enum": ["low", "medium", "high", "xhigh", "max"]}}}
            }}}),
            json!({"modelId": "claude-sonnet-4.5"}),
        ]);
        let payload = |model: &str, effort: Option<&str>| {
            let mut body = json!({"model": model, "messages": [{"role": "user", "content": "hi"}]});
            if let Some(effort) = effort {
                body["reasoning_effort"] = json!(effort);
            }
            let request: ChatCompletionRequest = serde_json::from_value(body).unwrap();
            build_kiro_payload(
                &request,
                "id",
                None,
                &Config::default(),
                &cache,
                &mut ToolNameAliases::default(),
            )
            .unwrap()
            .payload
        };
        let content = |payload: &Value| {
            payload["conversationState"]["currentMessage"]["userInputMessage"]["content"]
                .as_str()
                .unwrap()
                .to_owned()
        };

        let native = payload("claude-opus-5.5", Some("minimal"));
        assert_eq!(
            native["additionalModelRequestFields"],
            json!({
                "thinking": {"type": "adaptive", "display": "summarized"},
                "output_config": {"effort": "low"}
            })
        );
        assert!(!content(&native).contains("thinking"));

        let legacy = payload("claude-sonnet-4.5", Some("high"));
        assert!(legacy.get("additionalModelRequestFields").is_none());
        assert!(!content(&legacy).contains("thinking_mode"));
    }
}
