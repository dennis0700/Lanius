//! Emulation of Anthropic's server-side web search tool
//! (`web_search_20250305` and later) on top of Kiro's remote MCP
//! `web_search` tool (see [`crate::mcp`]).
//!
//! Anthropic executes server tools itself: the client sends the tool
//! definition and gets back `server_tool_use` / `web_search_tool_result`
//! blocks followed by the model's answer, all in one response. Kiro has no
//! such tool in its chat API, so Lanius runs the loop instead:
//!
//! 1. [`rewrite_request`] replaces the server tool with an ordinary
//!    `web_search` function tool and flattens prior-turn
//!    `server_tool_use` / `web_search_tool_result` blocks into text.
//! 2. [`run`] sends the request to Kiro; whenever the model calls
//!    `web_search`, it runs the query through Kiro's MCP endpoint, records
//!    the Anthropic-shaped blocks, appends the call and its results to the
//!    conversation, and asks the model to continue — until the model
//!    answers without searching, calls a client tool, or `max_uses` is hit.
//!
//! The finished turn is returned as a [`WebSearchOutcome`]; the caller
//! renders it as one JSON response or replays it as SSE frames.

use serde_json::{Map, Value, json};

use super::models::{
    AnthropicMessage, AnthropicMessageContent, AnthropicMessageRole, AnthropicMessagesRequest,
    AnthropicTool, ContentBlock, TextContentBlock, ToolResultContent, ToolResultContentBlock,
    ToolUseContentBlock,
};
use crate::error::{GatewayError, Result};
use crate::mcp::{McpHost, invoke_mcp};
use crate::upstream::{KiroHttpClient, StreamResult, ToolCall};

/// Name of the function tool the model sees in place of the server tool.
pub(crate) const TOOL_NAME: &str = "web_search";

/// Hard cap on search rounds when the client sets no `max_uses`, so a model
/// that keeps searching cannot loop forever.
const DEFAULT_MAX_USES: usize = 5;

/// Kiro's MCP `web_search` rejects queries longer than this.
const MAX_QUERY_CHARS: usize = 200;

const TOOL_DESCRIPTION: &str = "Search the web for up-to-date information. Returns a list of results \
     (title, url, snippet, publishedDate). Cite sources inline as markdown links.";

/// Returns `true` if the request offers Anthropic's server-hosted web
/// search tool (any tool whose `type` starts with `"web_search"`).
///
/// # Examples
///
/// ```ignore
/// assert!(is_web_search_request(&request));
/// ```
pub(crate) fn is_web_search_request(request: &AnthropicMessagesRequest) -> bool {
    server_tool(request).is_some()
}

fn server_tool(request: &AnthropicMessagesRequest) -> Option<&AnthropicTool> {
    request.tools.as_deref()?.iter().find(|tool| {
        tool.kind
            .as_deref()
            .is_some_and(|kind| kind.starts_with("web_search"))
    })
}

/// Settings carried over from the client's server-tool definition.
#[derive(Debug, Clone)]
pub(crate) struct WebSearchSettings {
    /// Maximum number of searches allowed for this turn.
    pub max_uses: usize,
    /// Only keep results from these domains (if set).
    pub allowed_domains: Option<Vec<String>>,
    /// Drop results from these domains.
    pub blocked_domains: Vec<String>,
}

/// Rewrites a web-search request into one Kiro can execute: the server tool
/// becomes a plain `web_search` function tool, and prior-turn server-tool
/// blocks (which Kiro does not understand) become text in their messages.
/// Returns the rewritten request plus the settings needed by [`run`].
pub(crate) fn rewrite_request(
    request: &AnthropicMessagesRequest,
) -> (AnthropicMessagesRequest, WebSearchSettings) {
    let settings = server_tool(request).map_or(
        WebSearchSettings {
            max_uses: DEFAULT_MAX_USES,
            allowed_domains: None,
            blocked_domains: Vec::new(),
        },
        |tool| WebSearchSettings {
            max_uses: tool
                .max_uses
                .and_then(|uses| usize::try_from(uses).ok())
                .filter(|uses| *uses > 0)
                .unwrap_or(DEFAULT_MAX_USES),
            allowed_domains: tool.allowed_domains.clone().filter(|d| !d.is_empty()),
            blocked_domains: tool.blocked_domains.clone().unwrap_or_default(),
        },
    );

    let mut rewritten = request.clone();
    let mut tools: Vec<AnthropicTool> = rewritten
        .tools
        .take()
        .unwrap_or_default()
        .into_iter()
        .filter(|tool| {
            !tool
                .kind
                .as_deref()
                .is_some_and(|kind| kind.starts_with("web_search"))
                && tool.name != TOOL_NAME
        })
        .collect();
    tools.push(function_tool());
    rewritten.tools = Some(tools);

    for message in &mut rewritten.messages {
        flatten_server_blocks(message);
    }
    (rewritten, settings)
}

fn function_tool() -> AnthropicTool {
    let schema = json!({
        "type": "object",
        "properties": {
            "query": {
                "type": "string",
                "description": "The search query. Must be 200 characters or fewer."
            }
        },
        "required": ["query"],
        "additionalProperties": false
    });
    AnthropicTool {
        kind: None,
        name: TOOL_NAME.to_owned(),
        description: Some(TOOL_DESCRIPTION.to_owned()),
        input_schema: schema.as_object().cloned(),
        max_uses: None,
        allowed_domains: None,
        blocked_domains: None,
        user_location: None,
    }
}

/// Replaces `server_tool_use` / `web_search_tool_result` blocks with text
/// summaries so the model still sees what was searched and found.
fn flatten_server_blocks(message: &mut AnthropicMessage) {
    let AnthropicMessageContent::Blocks(blocks) = &mut message.content else {
        return;
    };
    for block in blocks.iter_mut() {
        let text = match block {
            ContentBlock::ServerToolUse(tool) => {
                let query = tool
                    .input
                    .get("query")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                format!("[Searched the web for: {query}]")
            }
            ContentBlock::WebSearchToolResult(result) => {
                format!(
                    "[Web search results]\n{}",
                    result_blocks_to_text(result.content.as_ref())
                )
            }
            _ => continue,
        };
        *block = ContentBlock::Text(TextContentBlock {
            kind: Default::default(),
            text,
        });
    }
}

/// Renders client-echoed `web_search_result` blocks back into text, using
/// the snippet Lanius stashed in `encrypted_content` when available.
fn result_blocks_to_text(content: Option<&Value>) -> String {
    let Some(Value::Array(results)) = content else {
        return "(no results)".to_owned();
    };
    results
        .iter()
        .map(|result| {
            let title = result.get("title").and_then(Value::as_str).unwrap_or("");
            let url = result.get("url").and_then(Value::as_str).unwrap_or("");
            let snippet = result
                .get("encrypted_content")
                .and_then(Value::as_str)
                .and_then(|encoded| serde_json::from_str::<Value>(encoded).ok())
                .and_then(|decoded| decoded.get("snippet")?.as_str().map(str::to_owned))
                .unwrap_or_default();
            format!("- {title} ({url}): {snippet}")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// A completed web-search turn, ready to be rendered.
#[derive(Debug, Default)]
pub(crate) struct WebSearchOutcome {
    /// Anthropic content blocks in order: `server_tool_use` /
    /// `web_search_tool_result` pairs, any text the model wrote between
    /// searches, then the final turn's thinking/text/tool-use blocks.
    pub blocks: Vec<Value>,
    /// The last upstream round's raw result (for usage, truncation and the
    /// final `stop_reason`).
    pub last: StreamResult,
    /// Number of searches executed.
    pub searches: usize,
}

/// Runs one upstream round of the conversation. Supplied by the route
/// handler so this module stays independent of auth/payload plumbing.
pub(crate) trait Round {
    /// Sends `request` to Kiro and collects the full response.
    async fn send(&mut self, request: &AnthropicMessagesRequest) -> Result<StreamResult>;
}

/// Runs the search loop described in the module docs.
pub(crate) async fn run(
    mut request: AnthropicMessagesRequest,
    settings: &WebSearchSettings,
    round: &mut impl Round,
    auth: &crate::auth::AuthManager,
    http_client: &KiroHttpClient,
) -> Result<WebSearchOutcome> {
    let mut outcome = WebSearchOutcome::default();
    loop {
        let result = round.send(&request).await?;
        let (searches, others): (Vec<ToolCall>, Vec<ToolCall>) = result
            .tool_calls
            .iter()
            .cloned()
            .partition(|tool| tool.name == TOOL_NAME);

        // Stop when the model is done searching, also needs a client-side
        // tool (only the client can run it), or the search budget is spent.
        if searches.is_empty() || !others.is_empty() || outcome.searches >= settings.max_uses {
            let mut last = result;
            last.tool_calls.retain(|tool| tool.name != TOOL_NAME);
            outcome.last = last;
            return Ok(outcome);
        }

        if !result.content.trim().is_empty() {
            outcome
                .blocks
                .push(json!({"type": "text", "text": result.content}));
        }

        let mut assistant_blocks = Vec::new();
        if !result.content.trim().is_empty() {
            assistant_blocks.push(ContentBlock::Text(TextContentBlock {
                kind: Default::default(),
                text: result.content.clone(),
            }));
        }
        let mut result_blocks = Vec::new();
        for call in searches {
            let id = call
                .id
                .clone()
                .filter(|id| !id.is_empty())
                .unwrap_or_else(generate_server_tool_id);
            let query = query_from_arguments(&call.arguments);
            let server_id = format!("srvtoolu_{}", id.trim_start_matches("tooluse_"));

            let (anthropic_content, model_text) = if outcome.searches >= settings.max_uses {
                (
                    json!({"type": "web_search_tool_result_error", "error_code": "max_uses_exceeded"}),
                    "Error: the web search limit for this turn has been reached. Answer with the information you already have.".to_owned(),
                )
            } else {
                outcome.searches += 1;
                match search(auth, http_client, &query, settings).await {
                    Ok(results) => (
                        Value::Array(results.iter().map(to_anthropic_result).collect()),
                        results_for_model(&results),
                    ),
                    Err(error) => {
                        tracing::warn!(error = %error.log_detail(), "web_search MCP call failed");
                        (
                            json!({"type": "web_search_tool_result_error", "error_code": "unavailable"}),
                            "Error: web search is unavailable right now.".to_owned(),
                        )
                    }
                }
            };

            outcome.blocks.push(json!({
                "type": "server_tool_use",
                "id": server_id,
                "name": TOOL_NAME,
                "input": {"query": query},
            }));
            outcome.blocks.push(json!({
                "type": "web_search_tool_result",
                "tool_use_id": server_id,
                "content": anthropic_content,
            }));

            let mut input = Map::new();
            input.insert("query".to_owned(), Value::String(query));
            assistant_blocks.push(ContentBlock::ToolUse(ToolUseContentBlock {
                kind: Default::default(),
                id: id.clone(),
                name: TOOL_NAME.to_owned(),
                input,
            }));
            result_blocks.push(ContentBlock::ToolResult(ToolResultContentBlock {
                kind: Default::default(),
                tool_use_id: id,
                content: Some(ToolResultContent::Text(model_text)),
                is_error: None,
            }));
        }

        request.messages.push(AnthropicMessage {
            role: AnthropicMessageRole::Assistant,
            content: AnthropicMessageContent::Blocks(assistant_blocks),
        });
        request.messages.push(AnthropicMessage {
            role: AnthropicMessageRole::User,
            content: AnthropicMessageContent::Blocks(result_blocks),
        });
    }
}

fn generate_server_tool_id() -> String {
    let hex = uuid::Uuid::new_v4().simple().to_string();
    hex[..24].to_owned()
}

fn query_from_arguments(arguments: &str) -> String {
    let query = serde_json::from_str::<Value>(arguments)
        .ok()
        .and_then(|args| args.get("query")?.as_str().map(str::to_owned))
        .unwrap_or_default();
    query.chars().take(MAX_QUERY_CHARS).collect()
}

/// One search result as returned by Kiro's MCP `web_search`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
    /// Milliseconds since the Unix epoch.
    pub published_date: Option<i64>,
    pub domain: String,
}

/// Calls Kiro's MCP `web_search` and applies the client's domain filters.
async fn search(
    auth: &crate::auth::AuthManager,
    http_client: &KiroHttpClient,
    query: &str,
    settings: &WebSearchSettings,
) -> Result<Vec<SearchResult>> {
    let response = invoke_mcp(
        auth,
        http_client,
        McpHost::Runtime,
        "tools/call",
        Some(json!({"name": TOOL_NAME, "arguments": {"query": query}})),
    )
    .await?;
    let mut results = parse_mcp_response(&response)?;
    results.retain(|result| domain_allowed(&result.domain, &result.url, settings));
    Ok(results)
}

/// Extracts search results from an MCP `tools/call` response: the JSON-RPC
/// `result.content[]` text block holds a JSON document with a `results`
/// array.
pub(crate) fn parse_mcp_response(response: &Value) -> Result<Vec<SearchResult>> {
    if let Some(error) = response.get("error").filter(|error| !error.is_null()) {
        let message = error.get("message").and_then(Value::as_str).unwrap_or("");
        return Err(GatewayError::Internal(format!(
            "web_search MCP error: {message}"
        )));
    }
    let result = response.get("result").unwrap_or(&Value::Null);
    if result.get("isError").and_then(Value::as_bool) == Some(true) {
        return Err(GatewayError::Internal(
            "web_search MCP tool reported an error".into(),
        ));
    }
    let text = result
        .get("content")
        .and_then(Value::as_array)
        .and_then(|content| {
            content
                .iter()
                .find(|item| item.get("type").and_then(Value::as_str) == Some("text"))
        })
        .and_then(|item| item.get("text")?.as_str())
        .ok_or_else(|| GatewayError::Internal("web_search MCP response had no text".into()))?;
    let document: Value = serde_json::from_str(text)?;
    let string = |item: &Value, key: &str| {
        item.get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    Ok(document
        .get("results")
        .and_then(Value::as_array)
        .map(|results| {
            results
                .iter()
                .map(|item| SearchResult {
                    title: string(item, "title"),
                    url: string(item, "url"),
                    snippet: string(item, "snippet"),
                    published_date: item.get("publishedDate").and_then(Value::as_i64),
                    domain: string(item, "domain"),
                })
                .filter(|result| !result.url.is_empty())
                .collect()
        })
        .unwrap_or_default())
}

fn domain_allowed(domain: &str, url: &str, settings: &WebSearchSettings) -> bool {
    let host = url
        .split("://")
        .nth(1)
        .and_then(|rest| rest.split(['/', '?', '#']).next())
        .unwrap_or(domain)
        .to_ascii_lowercase();
    let matches = |pattern: &String| {
        let pattern = pattern.trim_start_matches("*.").to_ascii_lowercase();
        host == pattern || host.ends_with(&format!(".{pattern}"))
    };
    if settings.blocked_domains.iter().any(matches) {
        return false;
    }
    settings
        .allowed_domains
        .as_ref()
        .is_none_or(|allowed| allowed.iter().any(matches))
}

/// Maps one result to Anthropic's `web_search_result` block. The snippet is
/// stashed in `encrypted_content` (opaque to clients) so it can be restored
/// when the client echoes the block back in a later turn.
fn to_anthropic_result(result: &SearchResult) -> Value {
    let mut block = json!({
        "type": "web_search_result",
        "title": result.title,
        "url": result.url,
        "encrypted_content": json!({"snippet": result.snippet}).to_string(),
    });
    if let Some(age) = result.published_date.and_then(format_page_age) {
        block["page_age"] = Value::String(age);
    }
    block
}

fn format_page_age(millis: i64) -> Option<String> {
    chrono::DateTime::from_timestamp_millis(millis)
        .map(|date| date.format("%B %-d, %Y").to_string())
}

/// Formats results as the tool-result text the model reads.
fn results_for_model(results: &[SearchResult]) -> String {
    if results.is_empty() {
        return "No results found.".to_owned();
    }
    let items: Vec<Value> = results
        .iter()
        .map(|result| {
            json!({
                "title": result.title,
                "url": result.url,
                "snippet": result.snippet,
                "publishedDate": result.published_date.and_then(format_page_age),
            })
        })
        .collect();
    format!(
        "Found {} search result(s):\n{}",
        results.len(),
        serde_json::to_string_pretty(&items).unwrap_or_default()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(value: Value) -> AnthropicMessagesRequest {
        serde_json::from_value(value)
            .unwrap_or_else(|error| panic!("fixture request must deserialize: {error}"))
    }

    #[test]
    fn rewrite_swaps_server_tool_and_flattens_history() {
        let original = request(json!({
            "model": "claude", "max_tokens": 1,
            "tools": [
                {"type": "web_search_20250305", "name": "web_search", "max_uses": 3,
                 "blocked_domains": ["example.com"]},
                {"name": "read_file", "input_schema": {"type": "object"}}
            ],
            "messages": [
                {"role": "user", "content": "find it"},
                {"role": "assistant", "content": [
                    {"type": "server_tool_use", "id": "srvtoolu_1", "name": "web_search",
                     "input": {"query": "rust"}},
                    {"type": "web_search_tool_result", "tool_use_id": "srvtoolu_1", "content": [
                        {"type": "web_search_result", "title": "Rust", "url": "https://rust-lang.org",
                         "encrypted_content": "{\"snippet\":\"fast\"}"}
                    ]}
                ]},
                {"role": "user", "content": "more"}
            ]
        }));
        assert!(is_web_search_request(&original));
        let (rewritten, settings) = rewrite_request(&original);
        assert!(!is_web_search_request(&rewritten));
        assert_eq!(settings.max_uses, 3);
        assert_eq!(settings.blocked_domains, vec!["example.com".to_owned()]);
        let tools = rewritten.tools.unwrap_or_default();
        assert_eq!(tools.len(), 2);
        assert!(
            tools
                .iter()
                .any(|tool| tool.name == TOOL_NAME && tool.kind.is_none())
        );
        let AnthropicMessageContent::Blocks(blocks) = &rewritten.messages[1].content else {
            panic!("assistant content must stay as blocks");
        };
        let texts: Vec<&str> = blocks
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(texts.len(), 2);
        assert!(texts[0].contains("rust"));
        assert!(texts[1].contains("https://rust-lang.org") && texts[1].contains("fast"));
    }

    #[test]
    fn parses_live_mcp_response_shape() {
        let inner = json!({
            "results": [
                {"title": "Rust 1.90", "url": "https://blog.rust-lang.org/x", "snippet": "LLD default",
                 "publishedDate": 1758153600000_i64, "id": "1", "domain": "rust-lang.org",
                 "maxVerbatimWordLimit": 30, "publicDomain": true},
                {"title": "No date", "url": "https://example.org", "snippet": "s",
                 "publishedDate": null, "id": "2", "domain": "example.org"}
            ],
            "totalResults": 2, "query": "rust", "error": null
        });
        let response = json!({
            "error": null, "id": "1", "jsonrpc": "2.0",
            "result": {"content": [{"type": "text", "text": inner.to_string()}], "isError": false}
        });
        let results = parse_mcp_response(&response).unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].published_date, Some(1_758_153_600_000));
        assert_eq!(results[1].published_date, None);

        let block = to_anthropic_result(&results[0]);
        assert_eq!(block["type"], "web_search_result");
        assert_eq!(block["page_age"], "September 18, 2025");
        assert!(to_anthropic_result(&results[1]).get("page_age").is_none());
    }

    #[test]
    fn mcp_errors_are_reported() {
        let rpc_error = json!({"error": {"code": -1, "message": "boom"}, "result": null});
        assert!(parse_mcp_response(&rpc_error).is_err());
        let tool_error = json!({"error": null, "result": {"content": [], "isError": true}});
        assert!(parse_mcp_response(&tool_error).is_err());
    }

    #[test]
    fn domain_filters_match_subdomains() {
        let settings = WebSearchSettings {
            max_uses: 1,
            allowed_domains: Some(vec!["rust-lang.org".into()]),
            blocked_domains: vec!["blog.rust-lang.org".into()],
        };
        assert!(domain_allowed(
            "rust-lang.org",
            "https://doc.rust-lang.org/x",
            &settings
        ));
        assert!(!domain_allowed(
            "rust-lang.org",
            "https://blog.rust-lang.org/x",
            &settings
        ));
        assert!(!domain_allowed(
            "example.org",
            "https://example.org",
            &settings
        ));
    }

    struct FakeRound(Vec<StreamResult>);

    impl Round for FakeRound {
        async fn send(&mut self, _request: &AnthropicMessagesRequest) -> Result<StreamResult> {
            Ok(self.0.remove(0))
        }
    }

    #[tokio::test]
    async fn loop_returns_without_searching_when_model_answers_or_needs_client_tool() {
        let config = crate::Config::default();
        let auth = crate::auth::AuthManager::new(config.clone())
            .unwrap_or_else(|error| panic!("auth manager must construct: {error}"));
        let auth = std::sync::Arc::new(auth);
        let client = KiroHttpClient::new(auth.clone(), &config)
            .unwrap_or_else(|error| panic!("client must build: {error}"));
        let settings = WebSearchSettings {
            max_uses: 3,
            allowed_domains: None,
            blocked_domains: Vec::new(),
        };
        let base = request(json!({
            "model": "claude", "max_tokens": 1, "messages": [{"role": "user", "content": "hi"}]
        }));

        let mut answers = FakeRound(vec![StreamResult {
            content: "plain answer".into(),
            ..StreamResult::default()
        }]);
        let outcome = run(base.clone(), &settings, &mut answers, &auth, &client)
            .await
            .unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(outcome.searches, 0);
        assert!(outcome.blocks.is_empty());
        assert_eq!(outcome.last.content, "plain answer");

        // A client tool alongside a search ends the loop; the search call is
        // dropped because only the client can run its own tool.
        let tool = |name: &str| ToolCall {
            id: Some(format!("id_{name}")),
            name: name.into(),
            arguments: "{}".into(),
            truncation: None,
        };
        let mut mixed = FakeRound(vec![StreamResult {
            tool_calls: vec![tool(TOOL_NAME), tool("read_file")],
            ..StreamResult::default()
        }]);
        let outcome = run(base, &settings, &mut mixed, &auth, &client)
            .await
            .unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(outcome.searches, 0);
        let names: Vec<&str> = outcome
            .last
            .tool_calls
            .iter()
            .map(|t| t.name.as_str())
            .collect();
        assert_eq!(names, vec!["read_file"]);
    }

    #[test]
    fn queries_are_capped_at_kiro_limit() {
        let long = "x".repeat(300);
        let args = json!({"query": long}).to_string();
        assert_eq!(query_from_arguments(&args).chars().count(), MAX_QUERY_CHARS);
    }
}
