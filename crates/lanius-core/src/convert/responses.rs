//! Converts an OpenAI Responses API request directly into a Kiro request payload.
//!
//! Responses input items are translated straight into
//! [`super::core::UnifiedMessage`]/[`super::core::UnifiedTool`] and handed to
//! [`super::core::build_kiro_payload`], the same provider-agnostic core the Chat
//! Completions ([`super::openai`]) and Anthropic ([`super::anthropic`]) converters use.
//! No intermediate Chat Completions request is built.
//!
//! Item mapping:
//! - `instructions` and `system`/`developer` messages become the system prompt.
//! - `user`/`assistant` messages become turns (`input_text`/`output_text`/`refusal`
//!   parts become text, `input_image` data URLs become images).
//! - `function_call`/`custom_tool_call` become tool calls on the assistant turn, and
//!   `function_call_output`/`custom_tool_call_output` become tool results on the next
//!   user turn.
//! - `reasoning` items and built-in tool records are skipped (Kiro history has no slot
//!   for them); `item_reference` is rejected since Lanius stores no items.
//!
//! `custom` (freeform) tools are offered to Kiro as functions taking a single `input`
//! string; [`crate::api`] maps those calls back to `custom_tool_call` items.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

use serde_json::{Map, Value, json};

use super::core::{self, UnifiedMessage, UnifiedTool};
use super::openai::{flush_tool_results, nonempty_result, reasoning_request_from_effort};
use crate::api::{ResponsesInput, ResponsesRequest, ResponsesTool};
use crate::compat::ToolNameAliases;
use crate::config::Config;
use crate::error::{GatewayError, Result};
use crate::model::{ModelInfoCache, ReasoningRequest, get_model_id_for_kiro};

/// Name of the single string argument a `custom` tool is offered with.
const CUSTOM_TOOL_INPUT: &str = "input";

/// The namespace whose member tools keep their bare name (and that tool calls without a
/// `namespace` belong to).
const DEFAULT_TOOL_NAMESPACE: &str = "functions";

/// Maps a flattened tool name back to its `(namespace, name)` pair.
pub type ToolNamespaces = HashMap<String, (String, String)>;

/// Name of the function a client-executed `tool_search` tool is offered to Kiro as,
/// and the name Responses gives to its `tool_search_call` items.
pub const TOOL_SEARCH_TOOL_NAME: &str = "tool_search";

/// Default parameters of a `tool_search` tool that declares none.
fn tool_search_schema() -> Map<String, Value> {
    let schema = json!({
        "type": "object",
        "properties": {
            "query": {"type": "string", "description": "Search query for deferred tools."},
            "limit": {"type": "number", "description": "Maximum number of tools to return."},
        },
        "required": ["query"],
    });
    match schema {
        Value::Object(schema) => schema,
        _ => Map::new(),
    }
}

/// The flat name a namespaced tool is offered to Kiro under: the bare name for the
/// default `functions` namespace, otherwise the namespace and name joined with `__`.
///
/// # Examples
///
/// ```
/// use lanius_core::convert::namespaced_tool_name;
///
/// assert_eq!(namespaced_tool_name(None, "exec"), "exec");
/// assert_eq!(namespaced_tool_name(Some("functions"), "exec"), "exec");
/// assert_eq!(namespaced_tool_name(Some("collaboration"), "spawn_agent"), "collaboration__spawn_agent");
/// assert_eq!(namespaced_tool_name(Some("mcp__github__"), "search"), "mcp__github__search");
/// ```
pub fn namespaced_tool_name(namespace: Option<&str>, name: &str) -> String {
    match namespace
        .filter(|namespace| !namespace.is_empty() && *namespace != DEFAULT_TOOL_NAMESPACE)
    {
        None => name.to_owned(),
        Some(namespace) if namespace.ends_with('_') => format!("{namespace}{name}"),
        Some(namespace) => format!("{namespace}__{name}"),
    }
}

/// A request's tool declarations after [`flatten_responses_tools`].
#[derive(Debug, Default)]
pub struct FlattenedTools {
    /// `(namespace, name)` of namespaced tools, by flattened name.
    pub namespaces: ToolNamespaces,
    /// Whether a client-executed `tool_search` tool is offered (as the
    /// [`TOOL_SEARCH_TOOL_NAME`] function), so its calls map back to `tool_search_call`.
    pub tool_search: bool,
}

/// Normalizes a request's tool declarations into a flat `tools` list of `function` and
/// `custom` tools:
/// - tools declared in `additional_tools` input items (Codex's "Responses Lite" format)
///   are lifted into `tools` and the items removed;
/// - tools loaded by earlier `tool_search_output` items are added to `tools` (the items
///   stay, as the search call's result);
/// - `namespace` tools are replaced by their members under [`namespaced_tool_name`]s;
/// - a client-executed `tool_search` tool becomes the [`TOOL_SEARCH_TOOL_NAME`] function.
///
/// Later declarations of an already declared name are dropped.
///
/// # Examples
///
/// ```
/// use lanius_core::api::ResponsesRequest;
/// use lanius_core::convert::flatten_responses_tools;
/// use serde_json::json;
///
/// let mut request: ResponsesRequest = serde_json::from_value(json!({
///     "model": "m",
///     "input": [
///         {"type": "additional_tools", "role": "developer", "tools": [
///             {"type": "namespace", "name": "collaboration", "tools": [
///                 {"type": "function", "name": "spawn_agent", "parameters": {"type": "object"}}
///             ]}
///         ]},
///         {"role": "user", "content": "hi"}
///     ]
/// })).expect("valid request");
/// let flattened = flatten_responses_tools(&mut request);
/// let tools = request.tools.expect("lifted tools");
/// assert_eq!(tools[0].name.as_deref(), Some("collaboration__spawn_agent"));
/// assert_eq!(flattened.namespaces["collaboration__spawn_agent"].1, "spawn_agent");
/// assert!(!flattened.tool_search);
/// assert_eq!(request.input.into_items().len(), 1);
/// ```
pub fn flatten_responses_tools(request: &mut ResponsesRequest) -> FlattenedTools {
    let mut declared = request.tools.take().unwrap_or_default();
    if let ResponsesInput::Items(items) = &mut request.input {
        items.retain(|item| {
            let kind = str_field(item, "type");
            if !matches!(kind, Some("additional_tools" | "tool_search_output")) {
                return true;
            }
            let tools = item.get("tools").and_then(Value::as_array);
            declared.extend(
                tools
                    .into_iter()
                    .flatten()
                    .filter_map(|tool| serde_json::from_value::<ResponsesTool>(tool.clone()).ok()),
            );
            kind == Some("tool_search_output")
        });
    }
    let mut flattened = FlattenedTools::default();
    let mut flat = Vec::with_capacity(declared.len());
    for tool in declared {
        match tool.kind.as_str() {
            "namespace" => {
                let namespace = tool.name.unwrap_or_default();
                for mut member in tool.tools.unwrap_or_default() {
                    let Some(name) = member.name.take() else {
                        continue;
                    };
                    let flat_name = namespaced_tool_name(Some(&namespace), &name);
                    flattened
                        .namespaces
                        .insert(flat_name.clone(), (namespace.clone(), name));
                    member.name = Some(flat_name);
                    flat.push(member);
                }
            }
            "tool_search" if tool.execution.as_deref().unwrap_or("client") == "client" => {
                flattened.tool_search = true;
                flat.push(ResponsesTool {
                    kind: "function".to_owned(),
                    name: Some(TOOL_SEARCH_TOOL_NAME.to_owned()),
                    parameters: tool.parameters.or_else(|| Some(tool_search_schema())),
                    ..tool
                });
            }
            _ => flat.push(tool),
        }
    }
    let mut seen = HashSet::new();
    flat.retain(|tool| {
        tool.name
            .as_ref()
            .is_none_or(|name| seen.insert(name.clone()))
    });
    request.tools = (!flat.is_empty()).then_some(flat);
    flattened
}

/// Converts Responses input items into a `(system_prompt, unified_messages)` pair.
///
/// `system`/`developer` messages are pulled out and joined with `\n` into the system
/// prompt. Tool outputs are buffered and attached to a user turn before the next
/// non-output item (or at the end), mirroring the Chat Completions converter. Items
/// without a `type` but with a `role` are treated as messages.
///
/// # Examples
///
/// ```
/// use lanius_core::compat::ToolNameAliases;
/// use lanius_core::convert::convert_responses_input_to_unified;
/// use serde_json::json;
///
/// let items = vec![
///     json!({"role": "developer", "content": "be brief"}),
///     json!({"role": "user", "content": [{"type": "input_text", "text": "hi"}]}),
///     json!({"type": "function_call", "call_id": "c1", "name": "shell", "arguments": "{}"}),
///     json!({"type": "function_call_output", "call_id": "c1", "output": "ok"}),
/// ];
/// let (system, messages) =
///     convert_responses_input_to_unified(&items, &mut ToolNameAliases::default())
///         .expect("conversion succeeds");
/// assert_eq!(system, "be brief");
/// assert_eq!(messages.len(), 3);
/// assert_eq!(messages[2].tool_results[0]["tool_use_id"], "c1");
/// ```
pub fn convert_responses_input_to_unified(
    items: &[Value],
    aliases: &mut ToolNameAliases,
) -> Result<(String, Vec<UnifiedMessage>)> {
    let mut system_parts = Vec::new();
    let mut messages = Vec::new();
    let mut pending_results = Vec::new();
    let mut pending_images = Vec::new();
    for item in items {
        match item_type(item) {
            "message" => {
                let role = str_field(item, "role").unwrap_or("user");
                let content = normalize_content(item.get("content").unwrap_or(&Value::Null));
                if matches!(role, "system" | "developer") {
                    system_parts.push(core::extract_text_content(&content));
                    continue;
                }
                flush_tool_results(&mut messages, &mut pending_results, &mut pending_images);
                let mut message =
                    UnifiedMessage::new(role, Value::String(core::extract_text_content(&content)));
                if role != "assistant" {
                    message.images = core::extract_images_from_content(&content);
                }
                messages.push(message);
            }
            kind @ ("function_call" | "custom_tool_call" | "tool_search_call") => {
                if kind == "tool_search_call" && str_field(item, "execution") == Some("server") {
                    continue;
                }
                flush_tool_results(&mut messages, &mut pending_results, &mut pending_images);
                let arguments = match kind {
                    "custom_tool_call" => {
                        json!({CUSTOM_TOOL_INPUT: str_field(item, "input").unwrap_or_default()})
                            .to_string()
                    }
                    // `tool_search_call.arguments` is a JSON object, not a JSON string.
                    "tool_search_call" => match item.get("arguments") {
                        Some(Value::String(arguments)) => arguments.clone(),
                        Some(arguments) if !arguments.is_null() => arguments.to_string(),
                        _ => "{}".to_owned(),
                    },
                    _ => str_field(item, "arguments").unwrap_or("{}").to_owned(),
                };
                let name = if kind == "tool_search_call" {
                    TOOL_SEARCH_TOOL_NAME.to_owned()
                } else {
                    namespaced_tool_name(
                        str_field(item, "namespace"),
                        str_field(item, "name").unwrap_or_default(),
                    )
                };
                let call = json!({
                    "id": str_field(item, "call_id").unwrap_or_default(),
                    "type": "function",
                    "function": {
                        "name": aliases.alias_for(&name),
                        "arguments": arguments,
                    },
                });
                // Consecutive calls (and the text before them) form one assistant turn.
                match messages.last_mut().filter(|last| last.role == "assistant") {
                    Some(last) => last.tool_calls.push(call),
                    None => {
                        let mut message = UnifiedMessage::text("assistant", "");
                        message.tool_calls.push(call);
                        messages.push(message);
                    }
                }
            }
            "function_call_output" | "custom_tool_call_output" => {
                let output = normalize_content(item.get("output").unwrap_or(&Value::Null));
                pending_results.push(json!({
                    "type": "tool_result",
                    "tool_use_id": str_field(item, "call_id").unwrap_or_default(),
                    "content": nonempty_result(core::extract_text_content(&output)),
                }));
                pending_images.extend(core::extract_images_from_content(&output));
            }
            "tool_search_output" => {
                if str_field(item, "execution") == Some("server") {
                    continue;
                }
                pending_results.push(json!({
                    "type": "tool_result",
                    "tool_use_id": str_field(item, "call_id").unwrap_or_default(),
                    "content": tool_search_result_text(item, aliases),
                }));
            }
            "item_reference" => {
                return Err(GatewayError::InvalidRequest(
                    "item_reference input items are not supported: Lanius does not store \
                     responses; send the full items in `input`"
                        .to_owned(),
                ));
            }
            // `reasoning` and built-in tool records have no Kiro history equivalent.
            _ => {}
        }
    }
    flush_tool_results(&mut messages, &mut pending_results, &mut pending_images);
    Ok((system_parts.join("\n").trim().to_owned(), messages))
}

/// Converts Responses `tools` into unified tool definitions, aliasing each name via
/// `aliases`. `function` tools keep their schema; `custom` tools become a function
/// taking one `input` string, with any grammar appended to the description. Built-in
/// tool types are skipped. Returns `None` when no tool is usable.
///
/// # Examples
///
/// ```
/// use lanius_core::api::ResponsesTool;
/// use lanius_core::compat::ToolNameAliases;
/// use lanius_core::convert::convert_responses_tools_to_unified;
///
/// let tools: Vec<ResponsesTool> = serde_json::from_value(serde_json::json!([
///     {"type": "function", "name": "shell", "parameters": {"type": "object"}},
///     {"type": "web_search"}
/// ])).expect("valid tools");
/// let unified = convert_responses_tools_to_unified(Some(&tools), &mut ToolNameAliases::default())
///     .expect("one usable tool");
/// assert_eq!(unified.len(), 1);
/// assert_eq!(unified[0].name, "shell");
/// ```
pub fn convert_responses_tools_to_unified(
    tools: Option<&[ResponsesTool]>,
    aliases: &mut ToolNameAliases,
) -> Option<Vec<UnifiedTool>> {
    let converted = tools
        .unwrap_or_default()
        .iter()
        .filter_map(|tool| {
            let name = tool.name.as_deref()?;
            match tool.kind.as_str() {
                "function" => Some(UnifiedTool {
                    name: aliases.alias_for(name),
                    description: tool.description.clone(),
                    input_schema: tool.parameters.clone(),
                }),
                "custom" => Some(UnifiedTool {
                    name: aliases.alias_for(name),
                    description: Some(custom_tool_description(tool)),
                    input_schema: Some(custom_tool_schema()),
                }),
                _ => None,
            }
        })
        .collect::<Vec<_>>();
    (!converted.is_empty()).then_some(converted)
}

/// Normalizes a Responses request's `reasoning.effort` into a [`ReasoningRequest`],
/// with the same semantics as the Chat Completions `reasoning_effort` field.
///
/// # Examples
///
/// ```
/// use lanius_core::api::ResponsesRequest;
/// use lanius_core::convert::reasoning_request_from_responses;
/// use lanius_core::model::EffortLevel;
/// use serde_json::json;
///
/// let request: ResponsesRequest = serde_json::from_value(json!({
///     "model": "m", "input": "hi", "reasoning": {"effort": "low"}
/// })).expect("valid request");
/// assert_eq!(reasoning_request_from_responses(&request).effort, Some(EffortLevel::Low));
/// ```
pub fn reasoning_request_from_responses(request: &ResponsesRequest) -> ReasoningRequest {
    reasoning_request_from_effort(
        request
            .reasoning
            .as_ref()
            .and_then(|reasoning| reasoning.effort),
    )
}

/// Top-level entry point: converts a full [`ResponsesRequest`] into a Kiro request
/// payload. `instructions` is placed ahead of any `system`/`developer` input messages
/// in the system prompt. Tool names are registered before the input is converted so
/// history tool calls and tool declarations alias identically.
///
/// # Examples
///
/// ```
/// use lanius_core::api::ResponsesRequest;
/// use lanius_core::compat::ToolNameAliases;
/// use lanius_core::convert::responses_to_kiro;
/// use lanius_core::model::ModelInfoCache;
/// use lanius_core::Config;
/// use serde_json::json;
///
/// let request: ResponsesRequest = serde_json::from_value(json!({
///     "model": "claude-sonnet-4-5", "instructions": "be brief", "input": "hello"
/// })).expect("valid request");
/// let result = responses_to_kiro(
///     &request, "conv-1", None, &Config::default(), &ModelInfoCache::default(),
///     &mut ToolNameAliases::default(),
/// ).expect("conversion succeeds");
/// let content = result.payload["conversationState"]["currentMessage"]["userInputMessage"]
///     ["content"].as_str().unwrap_or_default();
/// assert!(content.contains("be brief") && content.contains("hello"));
/// ```
pub fn responses_to_kiro(
    request: &ResponsesRequest,
    conversation_id: &str,
    profile_arn: Option<&str>,
    config: &Config,
    model_cache: &ModelInfoCache,
    aliases: &mut ToolNameAliases,
) -> Result<core::KiroPayloadResult> {
    let tools = request.tools.as_deref();
    aliases.register_names(
        tools
            .unwrap_or_default()
            .iter()
            .filter(|tool| matches!(tool.kind.as_str(), "function" | "custom"))
            .filter_map(|tool| tool.name.as_deref()),
    );
    let items: Cow<'_, [Value]> = match &request.input {
        ResponsesInput::Items(items) => Cow::Borrowed(items),
        input @ ResponsesInput::Text(_) => Cow::Owned(input.clone().into_items()),
    };
    let (input_system, messages) = convert_responses_input_to_unified(&items, aliases)?;
    let instructions = request.instructions.as_deref().map(str::trim).unwrap_or("");
    let system = match (instructions.is_empty(), input_system.is_empty()) {
        (false, false) => format!("{instructions}\n{input_system}"),
        (false, true) => instructions.to_owned(),
        _ => input_system,
    };
    let tools = convert_responses_tools_to_unified(tools, aliases);
    let model = get_model_id_for_kiro(&request.model, &HashMap::new());
    let model_request_fields = model_cache
        .reasoning_capability(&model)
        .and_then(|capability| {
            capability.request_fields(&reasoning_request_from_responses(request))
        });
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

/// The tool result text for a `tool_search_output`: the flattened names (as offered to
/// Kiro, see [`flatten_responses_tools`]) and descriptions of the tools it loaded.
fn tool_search_result_text(item: &Value, aliases: &mut ToolNameAliases) -> String {
    let mut lines = Vec::new();
    let mut push = |namespace: Option<&str>, tool: &Value| {
        let Some(name) = str_field(tool, "name") else {
            return;
        };
        let name = aliases.alias_for(&namespaced_tool_name(namespace, name));
        match str_field(tool, "description")
            .map(str::trim)
            .filter(|d| !d.is_empty())
        {
            Some(description) => lines.push(format!("- {name}: {description}")),
            None => lines.push(format!("- {name}")),
        }
    };
    for tool in item
        .get("tools")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        match str_field(tool, "type") {
            Some("namespace") => {
                let namespace = str_field(tool, "name");
                for member in tool
                    .get("tools")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    push(namespace, member);
                }
            }
            _ => push(None, tool),
        }
    }
    if lines.is_empty() {
        "No matching tools were found.".to_owned()
    } else {
        format!(
            "These tools are now available to call:\n{}",
            lines.join("\n")
        )
    }
}

/// An item's `type`, defaulting to `message` (the Responses API allows omitting it on
/// role-bearing message items).
fn item_type(item: &Value) -> &str {
    str_field(item, "type").unwrap_or("message")
}

fn str_field<'a>(item: &'a Value, key: &str) -> Option<&'a str> {
    item.get(key).and_then(Value::as_str)
}

/// Rewrites Responses content parts into the block shapes [`core::extract_text_content`]
/// and [`core::extract_images_from_content`] understand. String content is returned as-is;
/// unsupported parts (e.g. `input_file`, `input_image` by `file_id`) are dropped.
fn normalize_content(content: &Value) -> Value {
    let Some(parts) = content.as_array() else {
        return content.clone();
    };
    Value::Array(
        parts
            .iter()
            .filter_map(|part| match str_field(part, "type") {
                Some("input_text" | "output_text" | "text") => {
                    Some(json!({"type": "text", "text": str_field(part, "text")?}))
                }
                Some("refusal") => {
                    Some(json!({"type": "text", "text": str_field(part, "refusal")?}))
                }
                Some("input_image") => {
                    let image_url = part.get("image_url")?;
                    let url = image_url.as_str().or_else(|| str_field(image_url, "url"))?;
                    Some(json!({"type": "image_url", "image_url": {"url": url}}))
                }
                None if part.is_string() => Some(part.clone()),
                _ => None,
            })
            .collect(),
    )
}

/// A `custom` tool's description, extended with its grammar (if any) so the model
/// knows what the freeform `input` must look like.
fn custom_tool_description(tool: &ResponsesTool) -> String {
    let mut description = tool.description.clone().unwrap_or_default();
    let grammar = tool
        .format
        .as_ref()
        .filter(|format| str_field(format, "type") == Some("grammar"));
    if let Some(definition) = grammar.and_then(|format| str_field(format, "definition")) {
        let syntax = grammar
            .and_then(|format| str_field(format, "syntax"))
            .unwrap_or("formal");
        description.push_str(&format!(
            "\n\nThe `{CUSTOM_TOOL_INPUT}` argument must follow this {syntax} grammar:\n{definition}"
        ));
    }
    description
}

/// The single-string-argument schema `custom` tools are offered to Kiro with.
fn custom_tool_schema() -> Map<String, Value> {
    let schema = json!({
        "type": "object",
        "properties": {CUSTOM_TOOL_INPUT: {
            "type": "string",
            "description": "The raw freeform input for this tool.",
        }},
        "required": [CUSTOM_TOOL_INPUT],
    });
    match schema {
        Value::Object(schema) => schema,
        _ => Map::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(body: Value) -> ResponsesRequest {
        serde_json::from_value(body).unwrap_or_else(|error| panic!("fixture: {error}"))
    }

    fn payload(request: &ResponsesRequest) -> Value {
        responses_to_kiro(
            request,
            "conv",
            None,
            &Config {
                truncation_recovery: false,
                ..Config::default()
            },
            &ModelInfoCache::default(),
            &mut ToolNameAliases::default(),
        )
        .unwrap_or_else(|error| panic!("conversion failed: {error}"))
        .payload
    }

    #[test]
    fn tool_round_trip_maps_to_kiro_history_and_tool_results() {
        let request = request(json!({
            "model": "claude-sonnet-4.5",
            "instructions": "sys",
            "input": [
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "list files"}]},
                {"type": "reasoning", "id": "rs_1", "summary": []},
                {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Running ls."}]},
                {"type": "function_call", "call_id": "call_1", "name": "shell", "arguments": "{\"cmd\": \"ls\"}"},
                {"type": "custom_tool_call", "call_id": "call_2", "name": "apply_patch", "input": "*** Begin Patch"},
                {"type": "function_call_output", "call_id": "call_1", "output": "a.txt"},
                {"type": "custom_tool_call_output", "call_id": "call_2", "output": [{"type": "input_text", "text": "done"}]}
            ],
            "tools": [
                {"type": "function", "name": "shell", "parameters": {"type": "object", "properties": {"cmd": {"type": "string"}}}},
                {"type": "custom", "name": "apply_patch", "description": "Patch files.", "format": {"type": "grammar", "syntax": "lark", "definition": "start: /.+/"}},
                {"type": "web_search"}
            ]
        }));
        let payload = payload(&request);
        let state = &payload["conversationState"];
        let history = state["history"].as_array().cloned().unwrap_or_default();
        assert_eq!(history.len(), 2);
        let first = history[0]["userInputMessage"]["content"]
            .as_str()
            .unwrap_or_default();
        assert!(first.starts_with("sys") && first.contains("list files"));
        let assistant = &history[1]["assistantResponseMessage"];
        assert_eq!(assistant["content"], "Running ls.");
        assert_eq!(assistant["toolUses"][0]["input"], json!({"cmd": "ls"}));
        assert_eq!(
            assistant["toolUses"][1]["input"],
            json!({"input": "*** Begin Patch"})
        );

        let context = &state["currentMessage"]["userInputMessage"]["userInputMessageContext"];
        let results = context["toolResults"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0]["toolUseId"], "call_1");
        assert_eq!(results[1]["content"][0]["text"], "done");
        let tools = context["tools"].as_array().cloned().unwrap_or_default();
        assert_eq!(tools.len(), 2);
        let custom = &tools[1]["toolSpecification"];
        assert_eq!(custom["name"], "apply_patch");
        assert!(
            custom["description"]
                .as_str()
                .is_some_and(|text| text.contains("lark grammar") && text.contains("start:"))
        );
        assert_eq!(custom["inputSchema"]["json"]["required"], json!(["input"]));
    }

    #[test]
    fn string_input_images_and_developer_messages() {
        let simple = payload(&request(json!({"model": "m", "input": "hello"})));
        assert_eq!(
            simple["conversationState"]["currentMessage"]["userInputMessage"]["content"],
            "hello"
        );

        let (system, messages) = convert_responses_input_to_unified(
            &[
                json!({"role": "system", "content": "a"}),
                json!({"role": "developer", "content": [{"type": "input_text", "text": "b"}]}),
                json!({"role": "user", "content": [
                    {"type": "input_text", "text": "看图"},
                    {"type": "input_image", "image_url": "data:image/png;base64,AA=="},
                    {"type": "input_file", "file_id": "f"}
                ]}),
            ],
            &mut ToolNameAliases::default(),
        )
        .unwrap_or_else(|error| panic!("conversion failed: {error}"));
        assert_eq!(system, "a\nb");
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].content, json!("看图"));
        assert_eq!(messages[0].images[0].media_type, "image/png");
    }

    #[test]
    fn additional_tools_namespaces_are_flattened_and_round_trip() {
        let mut request = request(json!({
            "model": "m",
            "tools": [],
            "input": [
                {"type": "additional_tools", "role": "developer", "tools": [
                    {"type": "namespace", "name": "functions", "tools": [
                        {"type": "custom", "name": "exec", "format": {"type": "text"}},
                        {"type": "function", "name": "wait", "parameters": {"type": "object"}}
                    ]},
                    {"type": "namespace", "name": "collaboration", "tools": [
                        {"type": "function", "name": "spawn_agent", "parameters": {"type": "object"}}
                    ]}
                ]},
                {"role": "user", "content": "go"},
                {"type": "function_call", "call_id": "c", "namespace": "collaboration", "name": "spawn_agent", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "c", "output": "ok"}
            ]
        }));
        let namespaces = flatten_responses_tools(&mut request).namespaces;
        assert_eq!(namespaces.len(), 3);
        let names: Vec<_> = request
            .tools
            .iter()
            .flatten()
            .filter_map(|tool| tool.name.as_deref())
            .collect();
        assert_eq!(names, ["exec", "wait", "collaboration__spawn_agent"]);

        let payload = payload(&request);
        let state = &payload["conversationState"];
        assert_eq!(
            state["history"][1]["assistantResponseMessage"]["toolUses"][0]["name"],
            "collaboration__spawn_agent"
        );
        let tools = state["currentMessage"]["userInputMessage"]["userInputMessageContext"]["tools"]
            .as_array()
            .map(Vec::len);
        assert_eq!(tools, Some(3));
    }

    #[test]
    fn tool_search_is_offered_and_history_round_trips() {
        let mut request = request(json!({
            "model": "m",
            "tools": [
                {"type": "function", "name": "exec_command", "parameters": {"type": "object"}},
                {"type": "tool_search", "execution": "client", "description": "Find tools.",
                 "parameters": {"type": "object", "properties": {"query": {"type": "string"}}}},
                {"type": "web_search"}
            ],
            "input": [
                {"role": "user", "content": "echo hi"},
                {"type": "tool_search_call", "call_id": "s1", "execution": "client",
                 "status": "completed", "arguments": {"query": "echo"}},
                {"type": "tool_search_output", "call_id": "s1", "execution": "client",
                 "status": "completed", "tools": [
                    {"type": "namespace", "name": "mcp__lt__", "description": "", "tools": [
                        {"type": "function", "name": "echo_text", "description": "Echo text.",
                         "parameters": {"type": "object"}}
                    ]}
                ]},
                {"type": "function_call", "call_id": "c1", "namespace": "mcp__lt__",
                 "name": "echo_text", "arguments": "{\"text\":\"hi\"}"},
                {"type": "function_call_output", "call_id": "c1", "output": "ECHO:hi"}
            ]
        }));
        let flattened = flatten_responses_tools(&mut request);
        assert!(flattened.tool_search);
        assert_eq!(flattened.namespaces["mcp__lt__echo_text"].1, "echo_text");
        let names: Vec<_> = request
            .tools
            .iter()
            .flatten()
            .filter_map(|tool| tool.name.as_deref())
            .collect();
        assert_eq!(names, ["exec_command", "tool_search", "mcp__lt__echo_text"]);

        let payload = payload(&request);
        let state = &payload["conversationState"];
        let history = state["history"].as_array().cloned().unwrap_or_default();
        let search = &history[1]["assistantResponseMessage"]["toolUses"][0];
        assert_eq!(search["name"], "tool_search");
        assert_eq!(search["input"], json!({"query": "echo"}));
        let found =
            history[2]["userInputMessage"]["userInputMessageContext"]["toolResults"][0]["content"]
                [0]["text"]
                .as_str()
                .unwrap_or_default();
        assert!(found.contains("mcp__lt__echo_text: Echo text."), "{found}");
        assert_eq!(
            history[3]["assistantResponseMessage"]["toolUses"][0]["name"],
            "mcp__lt__echo_text"
        );
    }

    #[test]
    fn server_tool_search_is_not_offered() {
        let mut request = request(json!({
            "model": "m", "input": "hi",
            "tools": [{"type": "tool_search", "execution": "server"}]
        }));
        assert!(!flatten_responses_tools(&mut request).tool_search);
        assert!(
            convert_responses_tools_to_unified(
                request.tools.as_deref(),
                &mut ToolNameAliases::default()
            )
            .is_none()
        );
    }

    #[test]
    fn item_reference_is_rejected() {
        let result = convert_responses_input_to_unified(
            &[json!({"type": "item_reference", "id": "msg_1"})],
            &mut ToolNameAliases::default(),
        );
        assert!(matches!(result, Err(GatewayError::InvalidRequest(_))));
    }

    #[test]
    fn long_tool_names_are_aliased_consistently() {
        let long = "mcp__plugin_everything_claude_code_github__create_pull_request_review";
        let request = request(json!({
            "model": "m",
            "input": [
                {"role": "user", "content": "go"},
                {"type": "function_call", "call_id": "c", "name": long, "arguments": "{}"},
                {"type": "function_call_output", "call_id": "c", "output": "ok"}
            ],
            "tools": [{"type": "function", "name": long, "parameters": {"type": "object"}}]
        }));
        let mut aliases = ToolNameAliases::default();
        let payload = responses_to_kiro(
            &request,
            "conv",
            None,
            &Config::default(),
            &ModelInfoCache::default(),
            &mut aliases,
        )
        .unwrap_or_else(|error| panic!("conversion failed: {error}"))
        .payload;
        let alias = aliases.alias_for(long);
        assert_ne!(alias, long);
        let state = &payload["conversationState"];
        assert_eq!(
            state["history"][1]["assistantResponseMessage"]["toolUses"][0]["name"],
            alias
        );
        assert_eq!(
            state["currentMessage"]["userInputMessage"]["userInputMessageContext"]["tools"][0]["toolSpecification"]
                ["name"],
            alias
        );
    }
}
