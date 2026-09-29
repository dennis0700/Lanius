//! Converts an Anthropic-shaped `Messages` API request into a Kiro request payload.
//!
//! This is the Anthropic-specific half of the [`super`] conversion pipeline (mirroring
//! [`super::openai`] for OpenAI-shaped requests): it translates `crate::api` Anthropic model
//! types into [`super::core::UnifiedMessage`]/[`super::core::UnifiedTool`], then delegates
//! to [`super::core::build_kiro_payload`] for the provider-agnostic normalization and
//! payload assembly.

use serde_json::Value;
use std::collections::HashMap;

use super::core::{self, UnifiedImage, UnifiedMessage, UnifiedTool};
use crate::api::{
    AnthropicContentBlock as ContentBlock, AnthropicImageSource as ImageSource, AnthropicMessage,
    AnthropicMessageContent, AnthropicMessageRole, AnthropicMessagesRequest, AnthropicTool,
    AnthropicToolResultContent as ToolResultContent, SystemPrompt,
};
use crate::compat::ToolNameAliases;
use crate::config::Config;
use crate::error::Result;
use crate::model::{EffortLevel, ModelInfoCache, ReasoningRequest, get_model_id_for_kiro};

/// Flattens an Anthropic message `content` (either a plain string or an array of content
/// blocks) down to its text-only portion, concatenating every `Text` block and dropping
/// any other block type (tool use, tool result, image).
///
/// # Examples
///
/// ```
/// use lanius_core::api::AnthropicMessageContent;
/// use lanius_core::convert::convert_anthropic_content_to_text;
///
/// let content = AnthropicMessageContent::Text("hello".into());
/// assert_eq!(convert_anthropic_content_to_text(&content), "hello");
/// ```
pub fn convert_anthropic_content_to_text(content: &AnthropicMessageContent) -> String {
    match content {
        AnthropicMessageContent::Text(text) => text.clone(),
        AnthropicMessageContent::Blocks(blocks) => blocks
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .collect(),
    }
}

/// Flattens an Anthropic `system` field (plain text, an array of typed blocks, or raw JSON
/// blocks parsed leniently) down to a single joined system prompt string. The three
/// [`SystemPrompt`] variants correspond to Anthropic's different accepted shapes for this
/// field; `RawBlocks` filters for `{"type":"text", "text":...}`-shaped entries and ignores
/// anything else, tolerating slightly malformed or unexpected block shapes rather than
/// erroring.
///
/// # Examples
///
/// ```
/// use lanius_core::api::SystemPrompt;
/// use lanius_core::convert::extract_system_prompt;
///
/// let system = SystemPrompt::Text("be concise".into());
/// assert_eq!(extract_system_prompt(Some(&system)), "be concise");
/// assert_eq!(extract_system_prompt(None), "");
/// ```
pub fn extract_system_prompt(system: Option<&SystemPrompt>) -> String {
    match system {
        None => String::new(),
        Some(SystemPrompt::Text(text)) => text.clone(),
        Some(SystemPrompt::Blocks(blocks)) => blocks
            .iter()
            .map(|block| block.text.as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        Some(SystemPrompt::RawBlocks(blocks)) => blocks
            .iter()
            .filter_map(|block| {
                block
                    .get("type")
                    .and_then(Value::as_str)
                    .filter(|kind| *kind == "text")
                    .and_then(|_| block.get("text"))
                    .and_then(Value::as_str)
            })
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

/// Converts a full Anthropic message list into unified messages, one-to-one (unlike the
/// OpenAI converter, Anthropic already groups a tool result with the user turn it belongs
/// to, so no buffering/flushing across multiple input messages is needed here — see
/// `convert_message`).
///
/// # Examples
///
/// ```
/// use lanius_core::api::{AnthropicMessage, AnthropicMessageContent, AnthropicMessageRole};
/// use lanius_core::compat::ToolNameAliases;
/// use lanius_core::convert::convert_anthropic_messages;
///
/// let messages = vec![AnthropicMessage {
///     role: AnthropicMessageRole::User,
///     content: AnthropicMessageContent::Text("hello".into()),
/// }];
/// let unified = convert_anthropic_messages(&messages, &mut ToolNameAliases::default())
///     .expect("conversion succeeds");
/// assert_eq!(unified[0].role, "user");
/// ```
pub fn convert_anthropic_messages(
    messages: &[AnthropicMessage],
    aliases: &mut ToolNameAliases,
) -> Result<Vec<UnifiedMessage>> {
    messages
        .iter()
        .map(|message| convert_message(message, aliases))
        .collect()
}

/// Converts Anthropic tool definitions into unified tool definitions, aliasing each name
/// via `aliases.alias_for` so it is Kiro-legal. Returns `None` (rather than `Some(vec![])`)
/// when there are no tools, matching [`super::core::build_kiro_payload`]'s "tools were not
/// offered" semantics.
///
/// # Examples
///
/// ```
/// use lanius_core::api::AnthropicTool;
/// use lanius_core::compat::ToolNameAliases;
/// use lanius_core::convert::convert_anthropic_tools;
///
/// let tools = vec![AnthropicTool {
///     kind: None,
///     name: "read_file".into(),
///     description: None,
///     input_schema: None,
///     max_uses: None,
///     allowed_domains: None,
///     blocked_domains: None,
///     user_location: None,
/// }];
/// let unified = convert_anthropic_tools(Some(&tools), &mut ToolNameAliases::default());
/// assert_eq!(unified.unwrap()[0].name, "read_file");
/// assert!(convert_anthropic_tools(None, &mut ToolNameAliases::default()).is_none());
/// ```
pub fn convert_anthropic_tools(
    tools: Option<&[AnthropicTool]>,
    aliases: &mut ToolNameAliases,
) -> Option<Vec<UnifiedTool>> {
    let converted = tools
        .unwrap_or_default()
        .iter()
        .map(|tool| UnifiedTool {
            name: aliases.alias_for(&tool.name),
            description: tool.description.clone(),
            input_schema: tool.input_schema.clone(),
        })
        .collect::<Vec<_>>();
    (!converted.is_empty()).then_some(converted)
}

/// Normalizes Anthropic's `thinking` (`{"type": "enabled"|"adaptive"|"disabled",
/// "display": ...}`) and `output_config.effort` request fields into a
/// [`ReasoningRequest`]. `budget_tokens` is ignored: the models that support native
/// thinking through Kiro use adaptive thinking, whose depth is controlled by effort.
///
/// # Examples
///
/// ```
/// use lanius_core::api::AnthropicMessagesRequest;
/// use lanius_core::convert::reasoning_request_from_anthropic;
/// use lanius_core::model::EffortLevel;
/// use serde_json::json;
///
/// let request: AnthropicMessagesRequest = serde_json::from_value(json!({
///     "model": "m", "max_tokens": 1, "messages": [],
///     "thinking": {"type": "disabled"}, "output_config": {"effort": "max"}
/// })).expect("valid request");
/// let reasoning = reasoning_request_from_anthropic(&request);
/// assert!(reasoning.disabled);
/// assert_eq!(reasoning.effort, Some(EffortLevel::Max));
/// ```
pub fn reasoning_request_from_anthropic(request: &AnthropicMessagesRequest) -> ReasoningRequest {
    let thinking = request.thinking.as_ref();
    ReasoningRequest {
        disabled: thinking
            .and_then(|thinking| thinking.get("type"))
            .and_then(Value::as_str)
            == Some("disabled"),
        effort: request
            .output_config
            .as_ref()
            .and_then(|config| config.get("effort"))
            .and_then(Value::as_str)
            .and_then(EffortLevel::parse),
        display: thinking
            .and_then(|thinking| thinking.get("display"))
            .and_then(Value::as_str)
            .map(str::to_owned),
    }
}

/// Top-level entry point: converts a full [`AnthropicMessagesRequest`] into a Kiro request
/// payload.
///
/// Tool names are registered with `aliases` up front (before message conversion), for the
/// same reason as the OpenAI path's `register_openai_tool_names` — so tool-call references
/// inside message history are aliased consistently with the top-level tool declarations.
/// Native reasoning fields are attached only for models whose catalog schema declares
/// them (see [`crate::model::ReasoningCapability`]).
///
/// # Examples
///
/// ```
/// use lanius_core::api::AnthropicMessagesRequest;
/// use lanius_core::compat::ToolNameAliases;
/// use lanius_core::convert::anthropic_to_kiro;
/// use lanius_core::model::ModelInfoCache;
/// use lanius_core::Config;
/// use serde_json::json;
///
/// let request: AnthropicMessagesRequest = serde_json::from_value(json!({
///     "model": "claude-sonnet-4-5", "max_tokens": 100,
///     "messages": [{"role": "user", "content": "hello"}]
/// })).expect("valid request");
/// let mut aliases = ToolNameAliases::default();
/// let cache = ModelInfoCache::default();
/// let result = anthropic_to_kiro(&request, "conv-1", None, &Config::default(), &cache, &mut aliases)
///     .expect("conversion succeeds");
/// assert!(result.payload["conversationState"]["currentMessage"].is_object());
/// ```
pub fn anthropic_to_kiro(
    request: &AnthropicMessagesRequest,
    conversation_id: &str,
    profile_arn: Option<&str>,
    config: &Config,
    model_cache: &ModelInfoCache,
    aliases: &mut ToolNameAliases,
) -> Result<core::KiroPayloadResult> {
    aliases.register_names(
        request
            .tools
            .as_deref()
            .unwrap_or_default()
            .iter()
            .map(|tool| tool.name.as_str()),
    );
    let messages = convert_anthropic_messages(&request.messages, aliases)?;
    let tools = convert_anthropic_tools(request.tools.as_deref(), aliases);
    let system = extract_system_prompt(request.system.as_ref());
    let model = get_model_id_for_kiro(&request.model, &HashMap::new());
    let model_request_fields = model_cache
        .reasoning_capability(&model)
        .and_then(|capability| {
            capability.request_fields(&reasoning_request_from_anthropic(request))
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

/// Converts one Anthropic message into a unified message: text content always, plus
/// (depending on role) tool uses extracted from an assistant's content blocks, or tool
/// results/images extracted from a user's content blocks. Plain-text (non-block) content
/// short-circuits immediately since there is nothing structured to extract from it.
fn convert_message(
    message: &AnthropicMessage,
    aliases: &mut ToolNameAliases,
) -> Result<UnifiedMessage> {
    let role = match message.role {
        AnthropicMessageRole::User => "user",
        AnthropicMessageRole::Assistant => "assistant",
        AnthropicMessageRole::System => "system",
    };
    let mut output =
        UnifiedMessage::text(role, convert_anthropic_content_to_text(&message.content));
    let AnthropicMessageContent::Blocks(blocks) = &message.content else {
        return Ok(output);
    };
    if role == "assistant" {
        output.tool_calls = extract_tool_uses(blocks, aliases);
    }
    if role == "user" {
        output.tool_results = extract_tool_results(blocks);
        output.images = extract_images_from_blocks(blocks);
        // Anthropic tool results can themselves embed images (a tool that returns a
        // screenshot, for instance); those need to be pulled out separately from the
        // top-level content blocks' own images.
        output
            .images
            .extend(extract_images_from_tool_results(blocks));
    }
    Ok(output)
}

/// Extracts `ToolUse` blocks (with non-empty id and name) from an assistant message's
/// content blocks, converting each into the unified OpenAI-shaped tool-call JSON form and
/// aliasing its name via `aliases.alias_for`. Blocks with an empty id or name are skipped
/// rather than producing a malformed tool call.
fn extract_tool_uses(blocks: &[ContentBlock], aliases: &mut ToolNameAliases) -> Vec<Value> {
    blocks.iter().filter_map(|block| match block { ContentBlock::ToolUse(tool) if !tool.id.is_empty() && !tool.name.is_empty() => Some(serde_json::json!({"id":tool.id,"type":"function","function":{"name":aliases.alias_for(&tool.name),"arguments":tool.input}})), _=>None }).collect()
}
/// Extracts `ToolResult` blocks (with a non-empty `tool_use_id`) from a user message's
/// content blocks, flattening each result's content to text via [`tool_result_text`] and
/// substituting `"(empty result)"` for an empty result.
fn extract_tool_results(blocks: &[ContentBlock]) -> Vec<Value> {
    blocks.iter().filter_map(|block| match block { ContentBlock::ToolResult(result) if !result.tool_use_id.is_empty() => { let text=result.content.as_ref().map(tool_result_text).unwrap_or_default(); Some(serde_json::json!({"type":"tool_result","tool_use_id":result.tool_use_id,"content":if text.is_empty(){"(empty result)"}else{&text}})) }, _=>None }).collect()
}
/// Flattens an Anthropic tool-result `content` value to plain text: a plain string passes
/// through, an array of blocks contributes only its `Text` blocks (mirroring
/// [`convert_anthropic_content_to_text`]'s behavior for message content), and any other
/// raw JSON value falls back to [`core::extract_text_content`].
fn tool_result_text(content: &ToolResultContent) -> String {
    match content {
        ToolResultContent::Text(text) => text.clone(),
        ToolResultContent::Blocks(blocks) => blocks
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .collect(),
        ToolResultContent::Other(value) => core::extract_text_content(value),
    }
}
/// Extracts every `Image` block's decoded data from a flat list of content blocks (see
/// [`image_from_block`]).
fn extract_images_from_blocks(blocks: &[ContentBlock]) -> Vec<UnifiedImage> {
    blocks.iter().filter_map(image_from_block).collect()
}
/// Extracts images nested inside `ToolResult` blocks' own content blocks — a tool result
/// can itself be a `Blocks` variant containing `Image` entries, which are not visible to
/// [`extract_images_from_blocks`]'s single-level scan since they are one level deeper.
fn extract_images_from_tool_results(blocks: &[ContentBlock]) -> Vec<UnifiedImage> {
    blocks
        .iter()
        .flat_map(|block| match block {
            ContentBlock::ToolResult(result) => match result.content.as_ref() {
                Some(ToolResultContent::Blocks(blocks)) => extract_images_from_blocks(blocks),
                _ => Vec::new(),
            },
            _ => Vec::new(),
        })
        .collect()
}
/// Extracts a [`UnifiedImage`] from a single `Image` block with a base64 source and
/// non-empty data; returns `None` for any other block type, or for an `Image` block whose
/// source is not the base64 variant (e.g. a URL-referenced image is not currently
/// supported here) or whose data is empty.
fn image_from_block(block: &ContentBlock) -> Option<UnifiedImage> {
    let ContentBlock::Image(image) = block else {
        return None;
    };
    match &image.source {
        ImageSource::Base64(source) if !source.data.is_empty() => Some(UnifiedImage {
            media_type: source.media_type.clone(),
            data: source.data.clone(),
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{
        AnthropicTextContentBlock as TextContentBlock,
        AnthropicToolResultContentBlock as ToolResultContentBlock,
        AnthropicToolUseContentBlock as ToolUseContentBlock,
    };
    use serde_json::Map;
    #[test]
    fn converts_text_tool_use_and_result() {
        let assistant = AnthropicMessage {
            role: AnthropicMessageRole::Assistant,
            content: AnthropicMessageContent::Blocks(vec![
                ContentBlock::Text(TextContentBlock {
                    kind: Default::default(),
                    text: "call".into(),
                }),
                ContentBlock::ToolUse(ToolUseContentBlock {
                    kind: Default::default(),
                    id: "x".into(),
                    name: "tool".into(),
                    input: Map::new(),
                }),
            ]),
        };
        let user = AnthropicMessage {
            role: AnthropicMessageRole::User,
            content: AnthropicMessageContent::Blocks(vec![ContentBlock::ToolResult(
                ToolResultContentBlock {
                    kind: Default::default(),
                    tool_use_id: "x".into(),
                    content: Some(ToolResultContent::Text("ok".into())),
                    is_error: Some(true),
                },
            )]),
        };
        let converted =
            convert_anthropic_messages(&[assistant, user], &mut ToolNameAliases::default())
                .unwrap();
        assert_eq!(converted[0].tool_calls.len(), 1);
        assert_eq!(converted[1].tool_results[0]["content"], "ok");
    }
    #[test]
    fn aliases_reserve_legal_tool_names_before_history_conversion() {
        let invalid = "has spaces";
        let legal_collision = ToolNameAliases::default().alias_for(invalid);
        let request: AnthropicMessagesRequest = serde_json::from_value(serde_json::json!({
            "model": "m", "max_tokens": 1,
            "messages": [
                {"role": "assistant", "content": [{"type": "tool_use", "id": "call", "name": legal_collision, "input": {}}]},
                {"role": "user", "content": "continue"}
            ],
            "tools": [
                {"name": invalid, "input_schema": {}},
                {"name": legal_collision, "input_schema": {}}
            ]
        }))
        .unwrap_or_else(|error| panic!("alias collision fixture must deserialize: {error}"));
        let config = Config {
            truncation_recovery: false,
            ..Default::default()
        };
        let mut aliases = ToolNameAliases::default();
        let payload = anthropic_to_kiro(
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
    fn system_blocks_join_with_newline() {
        let prompt = SystemPrompt::RawBlocks(vec![
            serde_json::json!({"type":"text","text":"a"}),
            serde_json::json!({"type":"text","text":"b"}),
        ]);
        assert_eq!(extract_system_prompt(Some(&prompt)), "a\nb");
    }
}
