//! Kiro's native `web_search` tool.
//!
//! Mirrors the Kiro CLI: a JSON-RPC `tools/call` request sent through the
//! `AmazonCodeWhispererStreamingService.InvokeMCP` operation on the Kiro
//! runtime host. The tool's results arrive as a JSON document embedded in
//! `result.content[0].text`, which is expanded here into
//! [`WebSearchResponse`]; the raw document is kept as well so it can be fed
//! back to the model verbatim as a tool result.

use reqwest::StatusCode;
use serde::Deserialize;
use serde_json::{Value, json};

use super::client::{KiroHttpClient, retry_delay, upstream_error_from_body};
use crate::config::MAX_RETRIES;
use crate::error::{GatewayError, Result, classify_network_error};
use crate::utils::{API_RUNTIME, API_STREAMING, kiro_headers_for};

const INVOKE_MCP_TARGET: &str = "AmazonCodeWhispererStreamingService.InvokeMCP";
const GET_PROFILE_TARGET: &str = "AmazonCodeWhispererService.GetProfile";

/// How long a `GetProfile` web-tools verdict is reused before asking again,
/// so a console change takes effect without restarting the gateway.
const WEB_TOOLS_POLICY_TTL: std::time::Duration = std::time::Duration::from_secs(300);

/// Cached result of the enterprise web-tools policy lookup, keyed by the
/// profile ARN it was fetched for.
static WEB_TOOLS_POLICY: std::sync::Mutex<Option<(String, std::time::Instant, bool)>> =
    std::sync::Mutex::new(None);

/// Upper bound on the MCP response body, guarding against an unbounded
/// upstream payload being buffered in memory.
const MAX_MCP_RESPONSE_BYTES: usize = 4 << 20;

/// The search results returned by Kiro's native `web_search` tool.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WebSearchResponse {
    /// Individual search hits, in upstream ranking order.
    #[serde(default)]
    pub results: Vec<WebSearchResult>,
    /// The query Kiro actually searched for.
    #[serde(default)]
    pub query: String,
    /// The full result document as Kiro returned it (including fields such
    /// as `domain` and `maxVerbatimWordLimit`), for handing back to the
    /// model as a tool result.
    #[serde(skip)]
    pub raw: Value,
}

/// A single search hit.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WebSearchResult {
    /// Page title.
    #[serde(default)]
    pub title: String,
    /// Page URL.
    #[serde(default)]
    pub url: String,
    /// Short excerpt of the page content.
    #[serde(default)]
    pub snippet: String,
    /// Publication time as Unix milliseconds, when known.
    #[serde(default)]
    pub published_date: Option<i64>,
}

/// Description of the model-facing `web_search` tool that Lanius executes
/// itself on the model's behalf.
pub const WEB_SEARCH_TOOL_DESCRIPTION: &str = "Search the web for information that is outside your training data or may have changed recently (versions, pricing, release notes, news, documentation of new libraries). Do not use it for well-established concepts or for things that can be answered from the conversation or codebase. Returns a JSON object with a `results` array; each result has `title`, `url`, `snippet`, `publishedDate` (Unix milliseconds) and `domain`. Prefer recent and official sources. When you use a result, cite it with an inline markdown link to its URL, and paraphrase instead of quoting more than about 30 consecutive words from any single source.";

/// JSON Schema of the model-facing `web_search` tool's input.
///
/// # Examples
///
/// ```
/// let schema = lanius_core::upstream::web_search_input_schema();
/// assert_eq!(schema["required"][0], "query");
/// ```
pub fn web_search_input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "query": {
                "type": "string",
                "maxLength": 200,
                "description": "Search query (max 200 chars); concise keywords, not full sentences"
            }
        },
        "required": ["query"]
    })
}

/// Why a server-executed web search produced no results. `code` uses
/// Anthropic's `web_search_tool_result_error` codes.
#[derive(Debug, Clone, PartialEq)]
pub struct WebSearchError {
    /// `invalid_tool_input`, `max_uses_exceeded` or `unavailable`.
    pub code: &'static str,
    /// Human-readable detail, also shown to the model.
    pub message: String,
}

/// One web search that Lanius executed on the model's behalf.
#[derive(Debug, Clone, PartialEq)]
pub struct WebSearchCall {
    /// The model's tool-use id for this call.
    pub tool_use_id: String,
    /// The query the model asked for.
    pub query: String,
    /// The search results, or why there are none.
    pub outcome: std::result::Result<WebSearchResponse, WebSearchError>,
}

impl KiroHttpClient {
    /// Whether the account's Kiro profile allows web tools.
    ///
    /// Kiro enforces the console's "Web search and web fetch tools" switch
    /// only in its clients (the `InvokeMCP` operation keeps answering), so
    /// the gateway applies it the same way the Kiro CLI does: accounts
    /// without a profile ARN (Builder ID, social login) always allow web
    /// tools; otherwise `GetProfile` is read and
    /// `profile.optInFeatures.webTools.toggle == "OFF"` disables them. A
    /// missing field, or a failed lookup, keeps them enabled, matching the
    /// Kiro CLI. Verdicts are cached for five minutes per profile.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use lanius_core::auth::AuthManager;
    /// use lanius_core::upstream::KiroHttpClient;
    /// use lanius_core::Config;
    /// use std::sync::Arc;
    ///
    /// # async fn example() -> lanius_core::Result<()> {
    /// let config = Config::default();
    /// let auth = Arc::new(AuthManager::new(config.clone())?);
    /// let client = KiroHttpClient::new(auth, &config)?;
    /// if !client.web_tools_enabled().await {
    ///     println!("web tools are disabled for this profile");
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub async fn web_tools_enabled(&self) -> bool {
        let auth = self.auth_manager();
        let Some(arn) = auth.profile_arn().await.filter(|arn| !arn.is_empty()) else {
            return true;
        };
        if let Some(enabled) = cached_policy(&arn) {
            return enabled;
        }
        let enabled = match self.fetch_web_tools_toggle(&arn).await {
            Ok(enabled) => enabled,
            Err(error) => {
                tracing::warn!(
                    error = %error.log_detail(),
                    "GetProfile failed; keeping web tools enabled"
                );
                return true;
            }
        };
        if !enabled {
            tracing::info!("web tools are disabled by the Kiro profile");
        }
        store_policy(arn, enabled);
        enabled
    }

    async fn fetch_web_tools_toggle(&self, arn: &str) -> Result<bool> {
        let auth = self.auth_manager();
        let url = format!("{}/", auth.control_plane_host().await);
        let body = serde_json::to_vec(&json!({"profileArn": arn}))?;
        let token = auth.access_token_and_autofetch().await?;
        let mut request = self.client().post(url).body(body);
        for (name, value) in kiro_headers_for(&token, API_RUNTIME) {
            request = request.header(name, value);
        }
        let response = request
            .header("x-amz-target", GET_PROFILE_TARGET)
            .send()
            .await?;
        let status = response.status();
        let bytes = response.bytes().await?;
        if status != StatusCode::OK {
            return Err(upstream_error_from_body(
                status,
                &String::from_utf8_lossy(&bytes),
            ));
        }
        Ok(web_tools_toggle(&serde_json::from_slice(&bytes)?))
    }
}

/// Reads `profile.optInFeatures.webTools.toggle`; anything other than
/// `"OFF"` (including a missing field) means enabled.
fn web_tools_toggle(profile: &Value) -> bool {
    profile
        .pointer("/profile/optInFeatures/webTools/toggle")
        .and_then(Value::as_str)
        != Some("OFF")
}

fn cached_policy(arn: &str) -> Option<bool> {
    let cache = WEB_TOOLS_POLICY
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    cache
        .as_ref()
        .filter(|(cached_arn, at, _)| cached_arn == arn && at.elapsed() < WEB_TOOLS_POLICY_TTL)
        .map(|(_, _, enabled)| *enabled)
}

fn store_policy(arn: String, enabled: bool) {
    *WEB_TOOLS_POLICY
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) =
        Some((arn, std::time::Instant::now(), enabled));
}

impl KiroHttpClient {
    /// Runs `query` through Kiro's native `web_search` MCP tool.
    ///
    /// Uses the `AmazonCodeWhispererStreamingService.InvokeMCP` operation on
    /// the Kiro runtime host, as the Kiro CLI does. Shares the chat client's
    /// retry policy: `403` forces a token
    /// refresh, `429`/`5xx` and retryable transport errors back off
    /// exponentially.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use lanius_core::auth::AuthManager;
    /// use lanius_core::upstream::KiroHttpClient;
    /// use lanius_core::Config;
    /// use std::sync::Arc;
    ///
    /// # async fn example() -> lanius_core::Result<()> {
    /// let config = Config::default();
    /// let auth = Arc::new(AuthManager::new(config.clone())?);
    /// let client = KiroHttpClient::new(auth, &config)?;
    /// let response = client.web_search("rust 2024 edition").await?;
    /// println!("{} results", response.results.len());
    /// # Ok(())
    /// # }
    /// ```
    pub async fn web_search(&self, query: &str) -> Result<WebSearchResponse> {
        let query = query.trim();
        if query.is_empty() {
            return Err(GatewayError::InvalidRequest(
                "web search query is empty".into(),
            ));
        }
        let auth = self.auth_manager();
        let profile_arn = auth.profile_arn().await.filter(|arn| !arn.is_empty());
        let url = format!("{}/", auth.api_host().await);
        let body = serde_json::to_vec(&mcp_request(query, profile_arn.as_deref()))?;

        let mut last_error = None;
        for attempt in 0..MAX_RETRIES {
            let token = auth.access_token_and_autofetch().await?;
            let mut request = self.client().post(&url).body(body.clone());
            for (name, value) in kiro_headers_for(&token, API_STREAMING) {
                request = request.header(name, value);
            }
            request = request.header("x-amz-target", INVOKE_MCP_TARGET);
            if let Some(arn) = &profile_arn {
                request = request.header("x-amzn-kiro-profile-arn", arn);
            }
            let last_attempt = attempt + 1 == MAX_RETRIES;
            match request.send().await {
                Ok(response) => {
                    let status = response.status();
                    let bytes = response.bytes().await?;
                    if bytes.len() > MAX_MCP_RESPONSE_BYTES {
                        return Err(GatewayError::Internal(format!(
                            "Kiro MCP response exceeds {MAX_MCP_RESPONSE_BYTES} bytes"
                        )));
                    }
                    if status == StatusCode::OK {
                        return parse_mcp_response(&bytes);
                    }
                    let error = upstream_error_from_body(status, &String::from_utf8_lossy(&bytes));
                    if status == StatusCode::FORBIDDEN && !last_attempt {
                        tracing::warn!(
                            attempt = attempt + 1,
                            "Kiro MCP returned 403; forcing token refresh"
                        );
                        auth.force_refresh().await?;
                        last_error = Some(error);
                        continue;
                    }
                    if (status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error())
                        && !last_attempt
                    {
                        tracing::warn!(
                            attempt = attempt + 1,
                            status = status.as_u16(),
                            "Kiro MCP returned retryable status; backing off"
                        );
                        tokio::time::sleep(retry_delay(attempt)).await;
                        last_error = Some(error);
                        continue;
                    }
                    return Err(error);
                }
                Err(error) => {
                    let info = classify_network_error(&error);
                    let retryable = info.is_retryable;
                    let error = GatewayError::Network(Box::new(info));
                    if !retryable || last_attempt {
                        return Err(error);
                    }
                    tokio::time::sleep(retry_delay(attempt)).await;
                    last_error = Some(error);
                }
            }
        }
        Err(last_error.unwrap_or_else(|| {
            GatewayError::Internal("Kiro MCP retry loop exited without a result".into())
        }))
    }
}

/// Builds the JSON-RPC `tools/call` envelope for the `web_search` tool,
/// with the profile ARN in the body as the Kiro CLI sends it.
fn mcp_request(query: &str, profile_arn: Option<&str>) -> Value {
    let mut request = json!({
        "id": format!("web_search_tooluse_{}", uuid::Uuid::new_v4().simple()),
        "jsonrpc": "2.0",
        "method": "tools/call",
        "params": {"name": "web_search", "arguments": {"query": query}}
    });
    if let Some(arn) = profile_arn {
        request["profileArn"] = Value::String(arn.to_string());
    }
    request
}

/// Unwraps the JSON-RPC response and parses the search results embedded as
/// JSON text in `result.content[0].text`.
fn parse_mcp_response(body: &[u8]) -> Result<WebSearchResponse> {
    let rpc: Value = serde_json::from_slice(body)?;
    if let Some(error) = rpc.get("error").filter(|error| !error.is_null()) {
        let code = error
            .get("code")
            .and_then(Value::as_i64)
            .unwrap_or_default();
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("unknown error");
        return Err(mcp_failure(format!("Kiro MCP error {code}: {message}")));
    }
    let result = rpc
        .get("result")
        .ok_or_else(|| mcp_failure("Kiro MCP response has no result".into()))?;
    if result.get("isError").and_then(Value::as_bool) == Some(true) {
        return Err(mcp_failure("Kiro web_search returned a tool error".into()));
    }
    let text = result
        .get("content")
        .and_then(Value::as_array)
        .and_then(|content| content.first())
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("text"))
        .and_then(|item| item.get("text"))
        .and_then(Value::as_str)
        .ok_or_else(|| mcp_failure("Kiro web_search result has no text content".into()))?;
    let raw: Value = serde_json::from_str(text)?;
    let mut response: WebSearchResponse = serde_json::from_value(raw.clone())?;
    response.raw = raw;
    Ok(response)
}

/// A failed-but-delivered MCP call, reported as a 502 from upstream.
fn mcp_failure(message: String) -> GatewayError {
    upstream_error_from_body(
        StatusCode::BAD_GATEWAY,
        &json!({"message": message}).to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn web_tools_toggle_defaults_to_enabled() {
        assert!(!web_tools_toggle(
            &json!({"profile": {"optInFeatures": {"webTools": {"toggle": "OFF"}}}})
        ));
        assert!(web_tools_toggle(
            &json!({"profile": {"optInFeatures": {"webTools": {"toggle": "ON"}}}})
        ));
        assert!(web_tools_toggle(
            &json!({"profile": {"optInFeatures": {"apiKeys": {"toggle": "OFF"}}}})
        ));
    }

    #[test]
    fn request_uses_json_rpc_tools_call() {
        let request = mcp_request("kiro", Some("arn:profile"));
        assert_eq!(request["profileArn"], "arn:profile");
        assert!(mcp_request("kiro", None).get("profileArn").is_none());
        assert_eq!(request["method"], "tools/call");
        assert_eq!(request["params"]["name"], "web_search");
        assert_eq!(request["params"]["arguments"]["query"], "kiro");
    }

    #[test]
    fn embedded_results_are_expanded() {
        let inner = json!({
            "results": [{"title": "Kiro", "url": "https://kiro.dev", "snippet": "IDE", "publishedDate": 1_700_000_000_000_i64}],
            "totalResults": 1,
            "query": "kiro"
        });
        let body = json!({"jsonrpc": "2.0", "result": {"content": [{"type": "text", "text": inner.to_string()}]}});
        let parsed = parse_mcp_response(body.to_string().as_bytes())
            .unwrap_or_else(|error| panic!("must parse: {error}"));
        assert_eq!(parsed.results.len(), 1);
        assert_eq!(parsed.results[0].url, "https://kiro.dev");
        assert_eq!(parsed.results[0].published_date, Some(1_700_000_000_000));
        assert_eq!(parsed.raw["totalResults"], 1);
    }

    #[test]
    fn rpc_and_tool_errors_are_reported() {
        let rpc = json!({"error": {"code": -32000, "message": "boom"}});
        assert!(parse_mcp_response(rpc.to_string().as_bytes()).is_err());
        let tool = json!({"result": {"isError": true, "content": []}});
        assert!(parse_mcp_response(tool.to_string().as_bytes()).is_err());
    }
}
