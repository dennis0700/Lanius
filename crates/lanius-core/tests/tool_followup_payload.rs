use lanius_core::ChatCompletionRequest;
use lanius_core::Config;
use lanius_core::compat::ToolNameAliases;
use lanius_core::convert::build_kiro_payload;
use lanius_core::model::ModelInfoCache;
use serde_json::Value;

const LONG_TOOL: &str = "get_current_weather_conditions_and_extended_forecast_for_a_named_city_v2";
const TOOL_USE_ID: &str = "tooluse_abc123";

fn followup_request() -> ChatCompletionRequest {
    let body = serde_json::json!({
        "model": "claude-sonnet-4-5",
        "tools": [{
            "type": "function",
            "function": {
                "name": LONG_TOOL,
                "description": "Get the current weather and forecast for a city.",
                "parameters": {
                    "type": "object",
                    "properties": {"city": {"type": "string"}},
                    "required": ["city"],
                },
            },
        }],
        "messages": [
            {"role": "user", "content": "What is the weather in Tokyo? Use the tool."},
            {"role": "assistant", "content": Value::Null, "tool_calls": [{
                "id": TOOL_USE_ID,
                "type": "function",
                "function": {"name": LONG_TOOL, "arguments": "{\"city\": \"Tokyo\"}"},
            }]},
            {"role": "tool", "tool_call_id": TOOL_USE_ID,
             "content": "{\"city\":\"Tokyo\",\"temp_c\":18,\"condition\":\"light rain\"}"},
        ],
    });
    serde_json::from_value(body).expect("request should deserialize")
}

fn build() -> (Value, ToolNameAliases) {
    let mut aliases = ToolNameAliases::default();
    let payload = build_kiro_payload(
        &followup_request(),
        "conv-1",
        None,
        &Config::default(),
        &ModelInfoCache::default(),
        &mut aliases,
    )
    .expect("payload should build")
    .payload;
    (payload, aliases)
}

#[test]
fn tool_definition_and_history_call_use_the_same_alias() {
    let (payload, aliases) = build();
    let state = &payload["conversationState"];

    let defined = state["currentMessage"]["userInputMessage"]["userInputMessageContext"]["tools"]
        [0]["toolSpecification"]["name"]
        .as_str()
        .expect("tool specification name");
    let called = state["history"]
        .as_array()
        .expect("history array")
        .iter()
        .find_map(|entry| entry["assistantResponseMessage"]["toolUses"][0]["name"].as_str())
        .expect("history tool use name");

    assert_eq!(
        defined, called,
        "the historical call must reference the same alias as the tool definition"
    );

    assert!(
        ToolNameAliases::needs_alias(LONG_TOOL),
        "fixture must exceed the upstream name limit to exercise aliasing"
    );
    assert_ne!(defined, LONG_TOOL, "an over-long name must be aliased");
    assert!(
        defined.len() <= 64,
        "alias must fit the upstream limit, got {} chars",
        defined.len()
    );
    assert_eq!(
        aliases.original_for(defined),
        LONG_TOOL,
        "the alias must map back to the client's original name"
    );
}

#[test]
fn tool_result_is_paired_to_the_clients_tool_use_id() {
    let (payload, _) = build();
    let results = &payload["conversationState"]["currentMessage"]["userInputMessage"]["userInputMessageContext"]
        ["toolResults"];

    assert_eq!(
        results[0]["toolUseId"].as_str(),
        Some(TOOL_USE_ID),
        "the tool result must keep the client's tool_call_id verbatim"
    );
    assert_eq!(results[0]["status"].as_str(), Some("success"));
    assert!(
        results[0]["content"][0]["text"]
            .as_str()
            .is_some_and(|text| text.contains("light rain")),
        "the tool output must reach the upstream intact"
    );
    // Kiro answers a placeholder text as if it were the user's message
    // instead of using the results, so a results-only turn stays empty, as
    // the Kiro CLI sends it.
    assert_eq!(
        payload["conversationState"]["currentMessage"]["userInputMessage"]["content"],
        ""
    );
}

#[test]
fn payload_identifies_as_the_kiro_cli_default_agent() {
    let (payload, _) = build();
    assert_eq!(payload["agentMode"], "default-v2");
    let state = &payload["conversationState"];
    assert_eq!(state["agentTaskType"], "vibe");
    assert_eq!(state["chatTriggerType"], "MANUAL");
    assert_eq!(state["conversationId"], "conv-1");
    assert_eq!(state["rootConversationId"], "conv-1");
    assert_eq!(
        state["agentContinuationId"].as_str().map(str::len),
        Some(36),
        "a fresh UUID continuation id"
    );
    // Field order of the Kiro CLI 3.5.0's `conversationState`.
    let keys: Vec<&str> = state
        .as_object()
        .expect("conversationState object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        [
            "currentMessage",
            "chatTriggerType",
            "conversationId",
            "history",
            "agentContinuationId",
            "agentTaskType",
            "rootConversationId"
        ]
    );

    let current = &state["currentMessage"]["userInputMessage"];
    assert_eq!(current["origin"], "KIRO_CLI");
    assert_eq!(current["modelId"], "claude-sonnet-4.5");
    assert!(
        current
            .pointer("/userInputMessageContext/envState")
            .is_none(),
        "the Kiro CLI no longer sends envState"
    );

    for entry in state["history"].as_array().expect("history array") {
        if let Some(user) = entry.get("userInputMessage") {
            assert_eq!(user["origin"], "KIRO_CLI");
            assert_eq!(
                user["modelId"], "claude-sonnet-4.5",
                "history user turns carry modelId"
            );
        }
    }
    assert!(!payload.to_string().contains("AI_EDITOR"));
    assert!(payload.get("systemPrompt").is_none());
}

#[test]
fn user_turn_fields_follow_the_kiro_cli_order() {
    let (payload, _) = build();
    let keys = |value: &Value| -> Vec<String> {
        value
            .as_object()
            .map(|object| object.keys().cloned().collect())
            .unwrap_or_default()
    };
    let current = &payload["conversationState"]["currentMessage"]["userInputMessage"];
    assert_eq!(
        keys(current),
        ["content", "userInputMessageContext", "origin", "modelId"]
    );
    assert_eq!(
        keys(&current["userInputMessageContext"]),
        ["toolResults", "tools"]
    );
    let history = payload["conversationState"]["history"]
        .as_array()
        .expect("history array");
    let first_user = history
        .iter()
        .find_map(|entry| entry.get("userInputMessage"))
        .expect("a history user turn");
    assert_eq!(keys(first_user), ["content", "origin", "modelId"]);
}

#[test]
fn system_prompt_opens_the_history_as_the_kiro_cli_sends_it() {
    let body = serde_json::json!({
        "model": "claude-sonnet-4-5",
        "messages": [
            {"role": "system", "content": "Be terse."},
            {"role": "user", "content": "hi"},
        ],
    });
    let request: ChatCompletionRequest = serde_json::from_value(body).expect("request");
    let config = Config {
        truncation_recovery: false,
        ..Config::default()
    };
    let payload = build_kiro_payload(
        &request,
        "conv-2",
        None,
        &config,
        &ModelInfoCache::default(),
        &mut ToolNameAliases::default(),
    )
    .expect("payload should build")
    .payload;
    let state = &payload["conversationState"];
    let history = state["history"].as_array().expect("history array");
    assert_eq!(history.len(), 2);
    assert_eq!(history[0]["userInputMessage"]["content"], "Be terse.");
    assert_eq!(
        history[1]["assistantResponseMessage"]["content"],
        "I will follow these instructions."
    );
    assert_eq!(state["currentMessage"]["userInputMessage"]["content"], "hi");
}
