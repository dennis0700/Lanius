//! Anthropic's server-side `web_search` tool, backed by Kiro's native
//! search.
//!
//! Two shapes are handled:
//! - Claude Code / Cowork run WebSearch as a dedicated sub-request: the only
//!   tool is `web_search_20250305` and the last user turn carries the query
//!   ([`is_search_subrequest`]). Such requests never reach the model; the
//!   results are returned directly as `server_tool_use` +
//!   `web_search_tool_result` + a text summary ([`WebSearchAnswer`]).
//! - Any other request offering the server tool gets a model-facing
//!   `web_search` tool instead ([`replace_server_tool`]) that Lanius runs on
//!   the model's behalf (see [`crate::api::search_loop`]); each executed
//!   search is reported with the same block pair ([`search_blocks`]).

use std::convert::Infallible;
use std::fmt::Write as _;

use axum::Json;
use axum::body::Body;
use axum::http::header;
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use serde_json::{Map, Value, json};

use super::models::{AnthropicMessageRole, AnthropicMessagesRequest, AnthropicTool, ToolChoice};
use super::sse::{format_sse_event, generate_message_id};
use crate::api::search_loop::{DEFAULT_MAX_SEARCHES, WEB_SEARCH_TOOL_NAME};
use crate::convert::convert_anthropic_content_to_text;
use crate::error::Result;
use crate::tokenizer::count_tokens;
use crate::upstream::{
    WEB_SEARCH_TOOL_DESCRIPTION, WebSearchCall, WebSearchResponse, WebSearchResult,
    web_search_input_schema,
};

/// Prefix Claude Code's WebSearch sub-request puts before the query.
const QUERY_PREFIX: &str = "Perform a web search for the query:";

/// Returns `true` for Anthropic's server-hosted web search tool
/// (`web_search_20250305` and later versions).
pub fn is_web_search_tool(tool: &AnthropicTool) -> bool {
    tool.kind
        .as_deref()
        .is_some_and(|kind| kind.starts_with("web_search"))
}

/// Returns `true` for a Claude Code / Cowork WebSearch sub-request: every
/// offered tool is a server web search tool, and the client either forces
/// that tool or uses Claude Code's fixed query prefix. These want the raw
/// results, not a model-written answer.
pub fn is_search_subrequest(request: &AnthropicMessagesRequest) -> bool {
    let only_search = request
        .tools
        .as_deref()
        .is_some_and(|tools| !tools.is_empty() && tools.iter().all(is_web_search_tool));
    if !only_search {
        return false;
    }
    let forced = matches!(
        &request.tool_choice,
        Some(ToolChoice::Tool(choice)) if request.tools.as_deref().is_some_and(|tools| {
            tools.iter().any(|tool| tool.name == choice.name)
        })
    );
    forced
        || request
            .messages
            .iter()
            .rev()
            .find(|message| message.role == AnthropicMessageRole::User)
            .is_some_and(|message| {
                convert_anthropic_content_to_text(&message.content)
                    .trim_start()
                    .starts_with(QUERY_PREFIX)
            })
}

/// Replaces server web search tools with a model-facing `web_search` tool
/// that Lanius executes. Returns the search budget (the tool's `max_uses`,
/// or [`DEFAULT_MAX_SEARCHES`]) when Lanius should run searches, or `None`
/// if the request offers no server search tool, or the client defines its
/// own `web_search` tool (whose calls must reach the client).
pub fn replace_server_tool(request: &mut AnthropicMessagesRequest) -> Option<usize> {
    let tools = request.tools.as_mut()?;
    let server = tools.iter().find(|tool| is_web_search_tool(tool))?;
    let budget = server
        .max_uses
        .and_then(|max| usize::try_from(max).ok())
        .unwrap_or(DEFAULT_MAX_SEARCHES);
    let server_names: Vec<String> = tools
        .iter()
        .filter(|tool| is_web_search_tool(tool))
        .map(|tool| tool.name.clone())
        .collect();
    tools.retain(|tool| !is_web_search_tool(tool));
    let client_owned = tools.iter().any(|tool| tool.name == WEB_SEARCH_TOOL_NAME);
    if !client_owned {
        tools.push(AnthropicTool {
            kind: None,
            name: WEB_SEARCH_TOOL_NAME.to_string(),
            description: Some(WEB_SEARCH_TOOL_DESCRIPTION.to_string()),
            input_schema: web_search_input_schema().as_object().cloned(),
            max_uses: None,
            allowed_domains: None,
            blocked_domains: None,
            user_location: None,
        });
    }
    // A forced choice of a removed server tool now targets its replacement,
    // or is dropped when there is none.
    if let Some(ToolChoice::Tool(choice)) = &mut request.tool_choice {
        if server_names.contains(&choice.name) {
            if client_owned {
                request.tool_choice = None;
            } else {
                choice.name = WEB_SEARCH_TOOL_NAME.to_string();
            }
        }
    }
    (!client_owned).then_some(budget)
}

/// Removes server web search tools when web tools are not allowed, so the
/// model works without them (as in the Kiro CLI). A forced choice of a
/// removed tool is dropped.
pub fn strip_server_tool(request: &mut AnthropicMessagesRequest) {
    let Some(tools) = request.tools.as_mut() else {
        return;
    };
    let removed: Vec<String> = tools
        .iter()
        .filter(|tool| is_web_search_tool(tool))
        .map(|tool| tool.name.clone())
        .collect();
    tools.retain(|tool| !is_web_search_tool(tool));
    if tools.is_empty() {
        request.tools = None;
    }
    if matches!(&request.tool_choice, Some(ToolChoice::Tool(choice)) if removed.contains(&choice.name))
    {
        request.tool_choice = None;
    }
}

/// Anthropic content blocks for one search Lanius executed: the
/// `server_tool_use` block and its `web_search_tool_result`, which carries
/// either the results or a `web_search_tool_result_error`.
pub fn search_blocks(call: &WebSearchCall) -> (Value, Value) {
    let id = server_tool_use_id(&call.tool_use_id);
    let content = match &call.outcome {
        Ok(response) => Value::Array(response.results.iter().map(result_block).collect()),
        Err(error) => json!({"type": "web_search_tool_result_error", "error_code": error.code}),
    };
    (
        json!({"type": "server_tool_use", "id": id, "name": WEB_SEARCH_TOOL_NAME, "input": {"query": call.query}}),
        json!({"type": "web_search_tool_result", "tool_use_id": id, "content": content}),
    )
}

/// Number of searches that actually reached Kiro, for
/// `usage.server_tool_use.web_search_requests`.
pub fn executed_count(calls: &[WebSearchCall]) -> usize {
    calls
        .iter()
        .filter(|call| {
            call.outcome
                .as_ref()
                .map_or_else(|error| error.code == "unavailable", |_| true)
        })
        .count()
}

/// Anthropic server tool ids carry a `srvtoolu_` prefix.
fn server_tool_use_id(id: &str) -> String {
    if id.starts_with("srvtoolu_") {
        id.to_string()
    } else {
        format!("srvtoolu_{id}")
    }
}

/// Extracts the search query from the last non-empty user turn, stripping
/// Claude Code's fixed sub-request prefix when present.
pub fn extract_query(request: &AnthropicMessagesRequest) -> Option<String> {
    request
        .messages
        .iter()
        .rev()
        .filter(|message| message.role == AnthropicMessageRole::User)
        .map(|message| convert_anthropic_content_to_text(&message.content))
        .map(|text| {
            let text = text.trim();
            text.strip_prefix(QUERY_PREFIX)
                .unwrap_or(text)
                .trim()
                .to_string()
        })
        .find(|text| !text.is_empty())
}

/// The assembled pieces of a web search answer, shared by the JSON and SSE
/// encoders.
pub struct WebSearchAnswer {
    model: String,
    tool_use_id: String,
    query: String,
    results: Vec<Value>,
    summary: String,
    input_tokens: usize,
    output_tokens: usize,
}

impl WebSearchAnswer {
    /// Builds the answer for `query` from Kiro's search `response`.
    pub fn new(
        model: &str,
        query: String,
        response: &WebSearchResponse,
        input_tokens: usize,
    ) -> Self {
        let summary = summary(&query, &response.results);
        Self {
            model: model.to_string(),
            tool_use_id: format!("srvtoolu_{}", uuid::Uuid::new_v4().simple()),
            results: response.results.iter().map(result_block).collect(),
            output_tokens: count_tokens(&summary, false),
            query,
            summary,
            input_tokens,
        }
    }

    fn usage(&self) -> Map<String, Value> {
        let mut usage = Map::new();
        usage.insert("input_tokens".into(), json!(self.input_tokens));
        usage.insert("output_tokens".into(), json!(self.output_tokens));
        usage.insert("server_tool_use".into(), json!({"web_search_requests": 1}));
        usage
    }

    /// Non-streaming `/v1/messages` response body.
    pub fn to_json(&self) -> Value {
        json!({
            "id": generate_message_id(),
            "type": "message",
            "role": "assistant",
            "content": [
                {"type": "server_tool_use", "id": self.tool_use_id, "name": "web_search", "input": {"query": self.query}},
                {"type": "web_search_tool_result", "tool_use_id": self.tool_use_id, "content": self.results},
                {"type": "text", "text": self.summary}
            ],
            "model": self.model,
            "stop_reason": "end_turn",
            "stop_sequence": Value::Null,
            "usage": self.usage()
        })
    }

    /// The complete SSE frame sequence for a streaming response.
    pub fn to_sse_frames(&self) -> Result<Vec<String>> {
        let input = serde_json::to_string(&json!({"query": self.query}))?;
        Ok(vec![
            format_sse_event(
                "message_start",
                &json!({"type": "message_start", "message": {
                    "id": generate_message_id(), "type": "message", "role": "assistant",
                    "content": [], "model": self.model, "stop_reason": Value::Null,
                    "stop_sequence": Value::Null,
                    "usage": {"input_tokens": self.input_tokens, "output_tokens": 0}
                }}),
            )?,
            format_sse_event(
                "content_block_start",
                &json!({"type": "content_block_start", "index": 0, "content_block": {
                    "type": "server_tool_use", "id": self.tool_use_id, "name": "web_search", "input": {}
                }}),
            )?,
            format_sse_event(
                "content_block_delta",
                &json!({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": input}}),
            )?,
            format_sse_event(
                "content_block_stop",
                &json!({"type": "content_block_stop", "index": 0}),
            )?,
            format_sse_event(
                "content_block_start",
                &json!({"type": "content_block_start", "index": 1, "content_block": {
                    "type": "web_search_tool_result", "tool_use_id": self.tool_use_id, "content": self.results
                }}),
            )?,
            format_sse_event(
                "content_block_stop",
                &json!({"type": "content_block_stop", "index": 1}),
            )?,
            format_sse_event(
                "content_block_start",
                &json!({"type": "content_block_start", "index": 2, "content_block": {"type": "text", "text": ""}}),
            )?,
            format_sse_event(
                "content_block_delta",
                &json!({"type": "content_block_delta", "index": 2, "delta": {"type": "text_delta", "text": self.summary}}),
            )?,
            format_sse_event(
                "content_block_stop",
                &json!({"type": "content_block_stop", "index": 2}),
            )?,
            format_sse_event(
                "message_delta",
                &json!({"type": "message_delta", "delta": {"stop_reason": "end_turn", "stop_sequence": Value::Null}, "usage": self.usage()}),
            )?,
            format_sse_event("message_stop", &json!({"type": "message_stop"}))?,
        ])
    }

    /// Encodes the answer as an HTTP response, streamed or not.
    pub fn into_response(self, stream: bool) -> Response {
        if !stream {
            return Json(self.to_json()).into_response();
        }
        let frames = match self.to_sse_frames() {
            Ok(frames) => frames,
            Err(error) => {
                tracing::error!(error = %error, "failed to encode web search SSE frames");
                return Json(self.to_json()).into_response();
            }
        };
        let body = futures_util::stream::iter(
            frames
                .into_iter()
                .map(|frame| Ok::<Bytes, Infallible>(Bytes::from(frame))),
        );
        (
            [
                (header::CONTENT_TYPE, "text/event-stream"),
                (header::CACHE_CONTROL, "no-cache"),
                (header::CONNECTION, "keep-alive"),
            ],
            Body::from_stream(body),
        )
            .into_response()
    }
}

/// Converts one Kiro hit to an Anthropic `web_search_result` block. The
/// snippet goes into `encrypted_content`, which clients treat as opaque and
/// echo back unchanged.
fn result_block(result: &WebSearchResult) -> Value {
    let page_age = result
        .published_date
        .filter(|millis| *millis > 0)
        .and_then(chrono::DateTime::from_timestamp_millis)
        .map(|date| date.format("%B %-d, %Y").to_string());
    json!({
        "type": "web_search_result",
        "title": result.title,
        "url": result.url,
        "encrypted_content": result.snippet,
        "page_age": page_age
    })
}

/// Plain-text rendering of the results so the model sees titles, URLs and
/// snippets even when the client only forwards text blocks.
fn summary(query: &str, results: &[WebSearchResult]) -> String {
    let mut text = format!("<web_search>\nSearch results for {query:?}:\n\n");
    if results.is_empty() {
        text.push_str("No results found.\n");
    }
    for (index, result) in results.iter().enumerate() {
        let _ = writeln!(text, "{}. {}", index + 1, result.title);
        if !result.url.is_empty() {
            let _ = writeln!(text, "   URL: {}", result.url);
        }
        if !result.snippet.is_empty() {
            let _ = writeln!(text, "   {}", result.snippet);
        }
        text.push('\n');
    }
    text.push_str("</web_search>");
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(value: Value) -> AnthropicMessagesRequest {
        serde_json::from_value(value)
            .unwrap_or_else(|error| panic!("fixture request must deserialize: {error}"))
    }

    fn search_request() -> AnthropicMessagesRequest {
        request(json!({
            "model": "claude", "max_tokens": 1,
            "messages": [{"role": "user", "content": "Perform a web search for the query: rust 2024"}],
            "tools": [{"type": "web_search_20250305", "name": "web_search", "max_uses": 8}]
        }))
    }

    fn response() -> WebSearchResponse {
        WebSearchResponse {
            results: vec![WebSearchResult {
                title: "Rust 2024".into(),
                url: "https://blog.rust-lang.org".into(),
                snippet: "Edition released".into(),
                published_date: Some(1_740_000_000_000),
            }],
            query: "rust 2024".into(),
            raw: Value::Null,
        }
    }

    #[test]
    fn detects_subrequest_and_extracts_query() {
        let search = search_request();
        assert!(is_search_subrequest(&search));
        assert_eq!(extract_query(&search).as_deref(), Some("rust 2024"));

        let plain = request(json!({
            "model": "claude", "max_tokens": 1,
            "messages": [{"role": "user", "content": "what's new in rust?"}],
            "tools": [{"type": "web_search_20250305", "name": "web_search"}]
        }));
        assert!(!is_search_subrequest(&plain));
        let forced = request(json!({
            "model": "claude", "max_tokens": 1,
            "messages": [{"role": "user", "content": "rust 2024"}],
            "tools": [{"type": "web_search_20250305", "name": "web_search"}],
            "tool_choice": {"type": "tool", "name": "web_search"}
        }));
        assert!(is_search_subrequest(&forced));
    }

    #[test]
    fn server_tool_is_replaced_by_executable_tool() {
        let mut mixed = request(json!({
            "model": "claude", "max_tokens": 1,
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [
                {"type": "web_search_20250305", "name": "web_search", "max_uses": 3},
                {"name": "read", "input_schema": {"type": "object"}}
            ],
            "tool_choice": {"type": "tool", "name": "web_search"}
        }));
        assert_eq!(replace_server_tool(&mut mixed), Some(3));
        let tools = mixed.tools.as_deref().unwrap_or_default();
        assert_eq!(tools.len(), 2);
        assert!(
            tools
                .iter()
                .all(|tool| tool.kind.is_none() && tool.input_schema.is_some())
        );
        assert!(
            matches!(&mixed.tool_choice, Some(ToolChoice::Tool(choice)) if choice.name == "web_search")
        );

        let mut client_owned = request(json!({
            "model": "claude", "max_tokens": 1,
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [
                {"type": "web_search_20250305", "name": "web_search"},
                {"name": "web_search", "input_schema": {"type": "object"}}
            ]
        }));
        assert_eq!(replace_server_tool(&mut client_owned), None);
        assert_eq!(client_owned.tools.as_ref().map(Vec::len), Some(1));
    }

    #[test]
    fn disabled_web_tools_strip_server_tool_and_forced_choice() {
        let mut forced = request(json!({
            "model": "claude", "max_tokens": 1,
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [
                {"type": "web_search_20250305", "name": "web_search"},
                {"name": "read", "input_schema": {"type": "object"}}
            ],
            "tool_choice": {"type": "tool", "name": "web_search"}
        }));
        strip_server_tool(&mut forced);
        assert_eq!(forced.tools.as_ref().map(Vec::len), Some(1));
        assert!(forced.tool_choice.is_none());
    }

    #[test]
    fn search_blocks_report_results_and_errors() {
        let ok = WebSearchCall {
            tool_use_id: "tooluse_1".into(),
            query: "rust".into(),
            outcome: Ok(response()),
        };
        let (call, result) = search_blocks(&ok);
        assert_eq!(call["id"], "srvtoolu_tooluse_1");
        assert_eq!(result["tool_use_id"], call["id"]);
        assert_eq!(result["content"][0]["type"], "web_search_result");
        let failed = WebSearchCall {
            outcome: Err(crate::upstream::WebSearchError {
                code: "max_uses_exceeded",
                message: String::new(),
            }),
            ..ok.clone()
        };
        assert_eq!(
            search_blocks(&failed).1["content"]["error_code"],
            "max_uses_exceeded"
        );
        assert_eq!(executed_count(&[ok, failed]), 1);
    }

    #[test]
    fn json_answer_pairs_tool_use_and_result() {
        let answer = WebSearchAnswer::new("claude", "rust 2024".into(), &response(), 10);
        let body = answer.to_json();
        assert_eq!(body["content"][0]["type"], "server_tool_use");
        assert_eq!(body["content"][1]["tool_use_id"], body["content"][0]["id"]);
        assert_eq!(
            body["content"][1]["content"][0]["url"],
            "https://blog.rust-lang.org"
        );
        assert_eq!(
            body["content"][1]["content"][0]["page_age"],
            "February 19, 2025"
        );
        assert!(
            body["content"][2]["text"]
                .as_str()
                .is_some_and(|t| t.contains("Edition released"))
        );
        assert_eq!(body["usage"]["server_tool_use"]["web_search_requests"], 1);
    }

    #[test]
    fn sse_answer_has_strict_block_order() {
        let answer = WebSearchAnswer::new("claude", "rust 2024".into(), &response(), 10);
        let frames = answer
            .to_sse_frames()
            .unwrap_or_else(|error| panic!("frames must encode: {error}"));
        let names: Vec<&str> = frames
            .iter()
            .filter_map(|frame| frame.strip_prefix("event: ")?.split('\n').next())
            .collect();
        assert_eq!(
            names,
            vec![
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "content_block_start",
                "content_block_stop",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop"
            ]
        );
        assert!(frames[1].contains("server_tool_use"));
        assert!(frames[4].contains("web_search_tool_result"));
    }
}
