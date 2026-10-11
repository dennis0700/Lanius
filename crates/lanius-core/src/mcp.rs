//! Client for Kiro's remote MCP endpoint (`KiroRuntimeService.InvokeMCP`,
//! `POST /mcp`), which hosts server-side tools such as `web_search`.
//!
//! The wire shape mirrors what the Kiro CLI's own remote-MCP client sends: a
//! REST-JSON body that is a plain JSON-RPC 2.0 request (`jsonrpc`, `id`,
//! `method`, optional `params`), with the profile ARN carried in the
//! `x-amzn-kiro-profile-arn` header. The response body is a JSON-RPC response
//! (`id`, `jsonrpc`, `result` or `error`).

use serde_json::{Value, json};

use crate::auth::AuthManager;
use crate::error::{GatewayError, Result, enhance_kiro_error};
use crate::upstream::KiroHttpClient;
use crate::utils::{API_STREAMING, kiro_headers_for};

/// Which Kiro host to send the MCP request to. The Kiro CLI uses the runtime
/// host (`runtime.<region>.kiro.dev`); the AWS control-plane host
/// (`q.<region>.amazonaws.com`) is kept as an alternative for comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpHost {
    /// `https://runtime.<region>.kiro.dev`
    Runtime,
    /// `https://q.<region>.amazonaws.com`
    ControlPlane,
}

/// Sends one JSON-RPC request to Kiro's `/mcp` endpoint and returns the raw
/// JSON-RPC response body (which may carry either `result` or `error`).
///
/// Non-200 HTTP statuses are returned as [`GatewayError::Upstream`].
///
/// # Examples
///
/// ```no_run
/// use lanius_core::auth::AuthManager;
/// use lanius_core::mcp::{McpHost, invoke_mcp};
/// use lanius_core::upstream::KiroHttpClient;
/// use lanius_core::Config;
/// use std::sync::Arc;
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let config = Config::default();
/// let auth = Arc::new(AuthManager::new(config.clone())?);
/// let client = KiroHttpClient::new(auth.clone(), &config)?;
/// let tools = invoke_mcp(&auth, &client, McpHost::Runtime, "tools/list", None).await?;
/// # let _ = tools;
/// # Ok(())
/// # }
/// ```
pub async fn invoke_mcp(
    auth: &AuthManager,
    http_client: &KiroHttpClient,
    host: McpHost,
    method: &str,
    params: Option<Value>,
) -> Result<Value> {
    let token = auth.access_token_and_autofetch().await?;
    let mut body = json!({
        "jsonrpc": "2.0",
        "id": uuid::Uuid::new_v4().to_string(),
        "method": method,
    });
    if let Some(params) = params {
        body["params"] = params;
    }

    let base = match host {
        McpHost::Runtime => auth.api_host().await,
        McpHost::ControlPlane => auth.control_plane_host().await,
    };
    let url = format!("{}/mcp", base.trim_end_matches('/'));

    let mut headers = reqwest::header::HeaderMap::new();
    for (name, value) in kiro_headers_for(&token, API_STREAMING) {
        if let Ok(value) = reqwest::header::HeaderValue::from_str(&value) {
            headers.insert(name, value);
        }
    }
    // REST-JSON operation: replace the awsJson content type set above.
    headers.insert(
        reqwest::header::CONTENT_TYPE,
        reqwest::header::HeaderValue::from_static("application/json"),
    );
    if let Some(profile_arn) = auth.profile_arn().await.filter(|arn| !arn.is_empty()) {
        if let Ok(value) = reqwest::header::HeaderValue::from_str(&profile_arn) {
            headers.insert("x-amzn-kiro-profile-arn", value);
        }
    }

    let response = http_client
        .client()
        .post(url)
        .headers(headers)
        .body(serde_json::to_vec(&body)?)
        .send()
        .await?;
    let status = response.status();
    let bytes = response.bytes().await?;
    let payload = serde_json::from_slice::<Value>(&bytes)
        .unwrap_or_else(|_| json!({"message": String::from_utf8_lossy(&bytes)}));
    if status != reqwest::StatusCode::OK {
        return Err(GatewayError::Upstream {
            status: status.as_u16(),
            info: Box::new(enhance_kiro_error(&payload)),
        });
    }
    Ok(payload)
}
