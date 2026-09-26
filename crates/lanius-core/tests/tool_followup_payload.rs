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
}
