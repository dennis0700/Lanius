//! `POST /v1/responses` handler.
//!
//! Mirrors the Chat Completions handler (bearer auth, API-key session
//! affinity and failover, truncation recovery, server-side `web_search`,
//! model resolution), but converts the request with
//! [`crate::convert::responses_to_kiro`] and formats Kiro's stream with
//! [`super::sse`].

use std::collections::HashSet;
use std::sync::Arc;

use axum::{
    Json,
    extract::{State, rejection::JsonRejection},
    http::HeaderMap,
    response::{IntoResponse, Response},
};
use serde_json::{Map, Value, json};

use super::super::routes::{
    OpenAiState, authorize, event_stream_response, gateway_error_response, validation_error,
    web_tools_allowed,
};
use super::super::sse::OpenAiFormatContext;
use super::models::{ResponsesInput, ResponsesRequest, ResponsesTool};
use super::sse::{ResponsesContext, collect_response, encode_responses_sse};
use crate::api::search_loop::{
    ByteStream, DEFAULT_MAX_SEARCHES, SearchLoop, WEB_SEARCH_TOOL_NAME, open_chat_stream,
};
use crate::auth::AuthManager;
use crate::compat::ToolNameAliases;
use crate::convert::{extract_text_content, flatten_responses_tools, responses_to_kiro};
use crate::error::GatewayError;
use crate::truncation::{
    TruncationStore, generate_truncation_user_message, prepend_tool_recovery_notice,
};
use crate::upstream::{WEB_SEARCH_TOOL_DESCRIPTION, web_search_input_schema};
use crate::utils::{HashableMessage, generate_conversation_id, hash_affinity_key};

/// Handles `POST /v1/responses`.
pub(crate) async fn create_response(
    State(state): State<OpenAiState>,
    headers: HeaderMap,
    request: std::result::Result<Json<ResponsesRequest>, JsonRejection>,
) -> Response {
    if let Err(response) = authorize(&headers, &state.config.proxy_api_key) {
        return *response;
    }
    let mut request = match request {
        Ok(Json(request)) => request,
        Err(rejection) => return validation_error(&rejection.to_string()),
    };
    if request.previous_response_id.is_some() {
        return gateway_error_response(GatewayError::InvalidRequest(
            "previous_response_id is not supported: Lanius does not store responses; \
             send the full conversation in `input`"
                .to_owned(),
        ));
    }
    if request.input.is_empty() {
        return validation_error("input must not be empty");
    }

    let original_model = request.model.clone();
    let tool_namespaces = flatten_responses_tools(&mut request);
    let echo = echo_fields(&request);
    let custom_tools = custom_tool_names(request.tools.as_deref());
    let request_tools = request.tools.as_deref().map(|tools| {
        tools
            .iter()
            .filter_map(|tool| serde_json::to_value(tool).ok())
            .collect::<Vec<_>>()
    });
    let mut items = std::mem::take(&mut request.input).into_items();
    let original_items = items.clone();
    let conversation_id = conversation_id_for(&items);
    let affinity_id = api_key_affinity_id(&headers, &request, &items, &conversation_id);
    let (active_key_index, initial_auth) = match state
        .auth_manager
        .api_key_manager_for_session(&affinity_id)
        .await
    {
        Ok(Some((index, auth))) => (Some(index), auth),
        Ok(None) => (None, state.auth_manager.clone()),
        Err(error) => return gateway_error_response(error),
    };
    inject_truncation_recovery(&mut items, &state.truncation_store, &conversation_id);
    request.input = ResponsesInput::Items(items);
    let web_tools_allowed =
        state.config.web_search_enabled && web_tools_allowed(&initial_auth, &state.config).await;
    let server_search = inject_web_search_tool(&mut request, web_tools_allowed);

    request.model = state.model_resolver.resolve(&request.model).internal_id;
    let (prepared, active_auth) = match prepare_with_failover(
        &state,
        &request,
        &conversation_id,
        &affinity_id,
        active_key_index,
        initial_auth,
    )
    .await
    {
        Ok(result) => result,
        Err(error) => return gateway_error_response(error),
    };
    let context = ResponsesContext {
        format: OpenAiFormatContext::new(
            original_model,
            (*state.model_cache).clone(),
            original_items,
            request_tools,
            state.config.clone(),
            conversation_id,
            state.truncation_store.clone(),
        )
        .with_tool_name_aliases(prepared.tool_name_aliases),
        custom_tools,
        tool_namespaces,
        echo,
    };
    let search = SearchLoop {
        auth: active_auth,
        config: state.config.clone(),
        max_searches: server_search.then_some(DEFAULT_MAX_SEARCHES),
    };
    let events = search.events(prepared.source, prepared.payload);
    if request.stream {
        event_stream_response(encode_responses_sse(events, context), None)
    } else {
        match collect_response(events, context).await {
            Ok(response) => Json(response).into_response(),
            Err(error) => gateway_error_response(error),
        }
    }
}

/// An open, preflighted upstream stream, the payload that produced it, and
/// the request's tool name aliases.
struct PreparedRequest {
    source: ByteStream,
    payload: Value,
    tool_name_aliases: ToolNameAliases,
}

async fn prepare_request(
    state: &OpenAiState,
    auth: &Arc<AuthManager>,
    request: &ResponsesRequest,
    conversation_id: &str,
) -> crate::error::Result<PreparedRequest> {
    auth.access_token_and_autofetch().await?;
    let profile_arn = auth
        .profile_arn()
        .await
        .or_else(|| state.config.profile_arn.clone());
    let mut tool_name_aliases = ToolNameAliases::default();
    let payload = responses_to_kiro(
        request,
        conversation_id,
        profile_arn.as_deref(),
        &state.config,
        &state.model_cache,
        &mut tool_name_aliases,
    )?
    .payload;
    let source = open_chat_stream(auth.clone(), &state.config, &payload).await?;
    Ok(PreparedRequest {
        source,
        payload,
        tool_name_aliases,
    })
}

/// Opens the upstream stream, switching to another configured Kiro API key
/// when the current one fails in a way that warrants failover.
async fn prepare_with_failover(
    state: &OpenAiState,
    request: &ResponsesRequest,
    conversation_id: &str,
    affinity_id: &str,
    mut key_index: Option<usize>,
    mut auth: Arc<AuthManager>,
) -> crate::error::Result<(PreparedRequest, Arc<AuthManager>)> {
    let mut attempted = Vec::new();
    loop {
        if let Some(index) = key_index {
            attempted.push(index);
        }
        let error = match prepare_request(state, &auth, request, conversation_id).await {
            Ok(prepared) => {
                if let Some(index) = key_index {
                    state
                        .auth_manager
                        .remember_api_key_session(affinity_id, index)
                        .await;
                }
                return Ok((prepared, auth));
            }
            Err(error) => error,
        };
        let Some(current) = key_index.filter(|_| error.should_failover_api_key()) else {
            return Err(error);
        };
        match state
            .auth_manager
            .failover_api_key_manager(current, &attempted, &error)
            .await?
        {
            Some((index, next_auth)) => {
                tracing::warn!(
                    from_key = current + 1,
                    to_key = index + 1,
                    "upstream request failed; switching API key for this session"
                );
                key_index = Some(index);
                auth = next_auth;
            }
            None => return Err(error),
        }
    }
}

/// Session affinity key: `x-session-id` header, then `prompt_cache_key`,
/// `user`, the first user item, and finally the conversation id.
fn api_key_affinity_id(
    headers: &HeaderMap,
    request: &ResponsesRequest,
    items: &[Value],
    fallback: &str,
) -> String {
    let nonempty = |value: &str| {
        let value = value.trim();
        (!value.is_empty()).then(|| value.to_owned())
    };
    let header_id = headers
        .get("x-session-id")
        .and_then(|value| value.to_str().ok())
        .and_then(nonempty);
    let request_id = request
        .prompt_cache_key
        .as_deref()
        .and_then(nonempty)
        .or_else(|| request.user.as_deref().and_then(nonempty));
    let first_user_item = items
        .iter()
        .find(|item| item.get("role").and_then(Value::as_str) == Some("user"))
        .or_else(|| items.first())
        .map(Value::to_string);
    hash_affinity_key(
        header_id
            .or(request_id)
            .or(first_user_item)
            .as_deref()
            .unwrap_or(fallback),
    )
}

/// Stable conversation id (truncation-store key) derived from the input
/// items, keyed by role (or item type) and content (or the whole item).
fn conversation_id_for(items: &[Value]) -> String {
    let hashable: Vec<HashableMessage<'_>> = items
        .iter()
        .map(|item| HashableMessage {
            role: item
                .get("role")
                .or_else(|| item.get("type"))
                .and_then(Value::as_str)
                .unwrap_or("user"),
            content: item.get("content").unwrap_or(item),
        })
        .collect();
    generate_conversation_id(&hashable)
}

/// Applies truncation recovery recorded by earlier turns: tool outputs for
/// truncated calls get a recovery notice prepended, and an assistant message
/// whose text was cut off is followed by a synthetic "continue" user message.
fn inject_truncation_recovery(
    items: &mut Vec<Value>,
    store: &TruncationStore,
    conversation_id: &str,
) {
    let original = std::mem::take(items);
    items.reserve(original.len());
    for mut item in original {
        let kind = item
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("message");
        let truncated_call = matches!(kind, "function_call_output" | "custom_tool_call_output")
            && item
                .get("call_id")
                .and_then(Value::as_str)
                .is_some_and(|id| store.get_tool_truncation(conversation_id, id).is_some());
        let content_truncated = kind == "message"
            && item.get("role").and_then(Value::as_str) == Some("assistant")
            && store
                .get_content_truncation(
                    conversation_id,
                    &extract_text_content(item.get("content").unwrap_or(&Value::Null)),
                )
                .is_some();
        if truncated_call {
            let original = extract_text_content(item.get("output").unwrap_or(&Value::Null));
            item["output"] = Value::String(prepend_tool_recovery_notice(&original));
        }
        items.push(item);
        if content_truncated {
            items.push(json!({
                "type": "message",
                "role": "user",
                "content": generate_truncation_user_message(),
            }));
        }
    }
}

/// Offers the model a Lanius-executed `web_search` function when `enabled`,
/// unless the client declares its own `web_search` function (whose calls
/// are then passed through). Returns whether the tool was added.
fn inject_web_search_tool(request: &mut ResponsesRequest, enabled: bool) -> bool {
    if !enabled {
        return false;
    }
    let tools = request.tools.get_or_insert_with(Vec::new);
    let client_owned = tools.iter().any(|tool| {
        matches!(tool.kind.as_str(), "function" | "custom")
            && tool.name.as_deref() == Some(WEB_SEARCH_TOOL_NAME)
    });
    if client_owned {
        return false;
    }
    tools.push(ResponsesTool {
        kind: "function".to_owned(),
        name: Some(WEB_SEARCH_TOOL_NAME.to_owned()),
        description: Some(WEB_SEARCH_TOOL_DESCRIPTION.to_owned()),
        parameters: web_search_input_schema().as_object().cloned(),
        strict: None,
        format: None,
        tools: None,
    });
    true
}

fn custom_tool_names(tools: Option<&[ResponsesTool]>) -> HashSet<String> {
    tools
        .unwrap_or_default()
        .iter()
        .filter(|tool| tool.kind == "custom")
        .filter_map(|tool| tool.name.clone())
        .collect()
}

/// Request parameters echoed on every `response` object, with the API's
/// defaults for omitted fields. `store` is always `false`: nothing is kept.
fn echo_fields(request: &ResponsesRequest) -> Map<String, Value> {
    let reasoning = request.reasoning.clone().unwrap_or_default();
    let fields = json!({
        "instructions": request.instructions,
        "max_output_tokens": request.max_output_tokens,
        "metadata": request.metadata.clone().unwrap_or_default(),
        "parallel_tool_calls": request.parallel_tool_calls.unwrap_or(true),
        "previous_response_id": Value::Null,
        "reasoning": {"effort": reasoning.effort, "summary": reasoning.summary},
        "store": false,
        "temperature": request.temperature.unwrap_or(1.0),
        "text": request.text.clone().unwrap_or_else(|| json!({"format": {"type": "text"}})),
        "tool_choice": request.tool_choice.clone().unwrap_or_else(|| json!("auto")),
        "tools": request.tools.clone().unwrap_or_default(),
        "top_p": request.top_p.unwrap_or(1.0),
        "truncation": request.truncation.as_deref().unwrap_or("disabled"),
        "user": request.user,
    });
    match fields {
        Value::Object(fields) => fields,
        _ => Map::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(body: Value) -> ResponsesRequest {
        serde_json::from_value(body).unwrap_or_else(|error| panic!("fixture: {error}"))
    }

    #[test]
    fn web_search_injection_respects_client_tools() {
        let mut plain = request(json!({"model": "m", "input": "hi"}));
        assert!(!inject_web_search_tool(&mut plain, false));
        assert!(inject_web_search_tool(&mut plain, true));
        assert!(!inject_web_search_tool(&mut plain, true));
        assert_eq!(plain.tools.as_ref().map(Vec::len), Some(1));

        // A built-in `web_search` declaration does not count as client-owned.
        let mut builtin =
            request(json!({"model": "m", "input": "hi", "tools": [{"type": "web_search"}]}));
        assert!(inject_web_search_tool(&mut builtin, true));
    }

    #[test]
    fn truncation_recovery_rewrites_outputs_and_continues_cut_text() {
        let mut items = vec![
            json!({"role": "user", "content": "go"}),
            json!({"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "half"}]}),
            json!({"type": "function_call_output", "call_id": "c", "output": "result"}),
        ];
        let conversation = conversation_id_for(&items);
        let store = TruncationStore::default();
        store.save_tool_truncation(&conversation, "c", "shell", json!({"reason": "cut"}));
        store.save_content_truncation(&conversation, "half");
        inject_truncation_recovery(&mut items, &store, &conversation);
        assert_eq!(items.len(), 4);
        assert_eq!(items[2]["role"], "user");
        assert!(
            items[3]["output"].as_str().is_some_and(
                |output| output.contains("[API Limitation]") && output.contains("result")
            )
        );
    }

    #[test]
    fn echo_defaults_and_affinity_prefers_prompt_cache_key() {
        let request = request(json!({
            "model": "m", "input": "hi", "prompt_cache_key": "session-1",
            "tools": [{"type": "custom", "name": "apply_patch"}]
        }));
        let echo = echo_fields(&request);
        assert_eq!(echo["store"], false);
        assert_eq!(echo["tool_choice"], "auto");
        assert_eq!(echo["parallel_tool_calls"], true);
        assert_eq!(echo["tools"][0]["name"], "apply_patch");
        assert!(custom_tool_names(request.tools.as_deref()).contains("apply_patch"));

        let items = request.input.clone().into_items();
        assert_eq!(
            api_key_affinity_id(&HeaderMap::new(), &request, &items, "fallback"),
            hash_affinity_key("session-1")
        );
    }
}
