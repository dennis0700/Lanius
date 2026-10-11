//! Implements the `mcp <list|call>` subcommand: sends a raw JSON-RPC request
//! to Kiro's remote MCP endpoint (`POST /mcp`) and prints the raw JSON-RPC
//! response, for inspecting server-hosted tools such as `web_search`.

use std::sync::Arc;

use anyhow::{Context, Result};
use lanius_core::Config;
use lanius_core::auth::AuthManager;
use lanius_core::mcp::{McpHost, invoke_mcp};
use lanius_core::upstream::KiroHttpClient;

/// Implements the `mcp` subcommand.
///
/// Parameters:
/// - `config`: validated gateway configuration (region, auth, etc.).
/// - `tool`: `None` sends `tools/list`; `Some((name, args_json))` sends
///   `tools/call` with `{"name": name, "arguments": args_json}`.
/// - `control_plane`: send to `q.<region>.amazonaws.com` instead of the
///   runtime host the Kiro CLI uses.
///
/// Side effects: network I/O to obtain/refresh an access token and call the
/// MCP endpoint; prints the pretty-printed JSON-RPC response to stdout.
///
/// Errors if config validation fails, `args_json` is not valid JSON,
/// auth/token retrieval fails, or the upstream request fails.
pub(crate) async fn run(
    config: Config,
    tool: Option<(String, String)>,
    control_plane: bool,
) -> Result<()> {
    config.validate().map_err(|e| anyhow::anyhow!("{e}"))?;

    let auth = Arc::new(AuthManager::new(config.clone()).map_err(|e| anyhow::anyhow!("{e}"))?);
    let client = KiroHttpClient::new(auth.clone(), &config).map_err(|e| anyhow::anyhow!("{e}"))?;
    let host = if control_plane {
        McpHost::ControlPlane
    } else {
        McpHost::Runtime
    };

    let (method, params) = match tool {
        None => ("tools/list", None),
        Some((name, args)) => {
            let arguments: serde_json::Value =
                serde_json::from_str(&args).context("tool arguments must be valid JSON")?;
            (
                "tools/call",
                Some(serde_json::json!({"name": name, "arguments": arguments})),
            )
        }
    };

    let response = invoke_mcp(&auth, &client, host, method, params)
        .await
        .map_err(|e| anyhow::anyhow!("MCP request failed: {}", e.log_detail()))?;
    println!("{}", serde_json::to_string_pretty(&response)?);
    Ok(())
}
