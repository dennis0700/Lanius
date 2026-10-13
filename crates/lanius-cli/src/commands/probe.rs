//! Implements the `probe [prompt] [--capture <file>]` subcommand: performs
//! a live end-to-end request against the real Kiro backend using the same
//! auth/HTTP-client machinery as the gateway itself, then streams and
//! decodes the response with the shared event parser.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use futures_util::StreamExt;
use lanius_core::Config;
use lanius_core::auth::AuthManager;
use lanius_core::upstream::{KiroHttpClient, parse_kiro_stream};

use super::events::print_events;

/// Implements the `probe [prompt] [--capture <file>]` subcommand: performs a
/// live end-to-end request against the real Kiro backend using the same
/// [`AuthManager`] / [`KiroHttpClient`] machinery as the gateway itself, then
/// streams and decodes the response with [`parse_kiro_stream`].
///
/// Parameters:
/// - `config`: validated gateway configuration (region, auth, timeouts, etc.).
/// - `prompt`: the user message text sent as the sole turn of a new
///   conversation.
/// - `capture`: optional filesystem path; when present, the raw upstream
///   response bytes are buffered in memory while streaming and written to
///   this path once the stream completes, producing a fixture consumable by
///   the `replay` subcommand.
///
/// Side effects:
/// - Network I/O: obtains/refreshes an access token, then issues a POST to
///   the Kiro `generateAssistantResponse` endpoint and reads the streamed
///   response.
/// - Prints auth/account diagnostics and decoded stream events to stdout.
/// - File I/O: if `capture` is set, creates parent directories as needed and
///   writes the captured raw bytes to disk.
///
/// Errors if config validation fails, auth/token retrieval fails, the
/// upstream request fails, the response stream fails to parse, or the
/// capture file cannot be created/written.
pub(crate) async fn run(config: Config, prompt: String, capture: Option<PathBuf>) -> Result<()> {
    config.validate().map_err(|e| anyhow::anyhow!("{e}"))?;

    let auth = Arc::new(AuthManager::new(config.clone()).map_err(|e| anyhow::anyhow!("{e}"))?);

    println!("auth type   : {:?}", auth.auth_type().await);
    println!("region      : {}", auth.region().await);
    println!("fingerprint : {}", auth.fingerprint());
    match auth.profile_arn().await {
        Some(arn) => println!("profile arn : {arn}"),
        None if auth.is_api_key() => println!("profile arn : (n/a — API key)"),
        None => println!("profile arn : (none — Builder ID account)"),
    }

    let token = auth
        .access_token()
        .await
        .map_err(|e| anyhow::anyhow!("failed to obtain access token: {e}"))?;
    println!("access token: obtained ({} chars)\n", token.len());

    let client = KiroHttpClient::new(auth.clone(), &config).map_err(|e| anyhow::anyhow!("{e}"))?;

    let conversation_state = serde_json::json!({
        "chatTriggerType": "MANUAL",
        "conversationId": uuid_v4(),
        "currentMessage": {
            "userInputMessage": {
                "content": prompt,
                "modelId": "auto",
                "origin": lanius_core::utils::KIRO_ORIGIN,
            }
        },
        "history": [],
    });
    let mut payload = serde_json::json!({ "conversationState": conversation_state });
    if let Some(arn) = auth.profile_arn().await.filter(|arn| !arn.is_empty()) {
        payload["profileArn"] = serde_json::Value::String(arn);
    }

    let url = config.generate_assistant_response_url();
    println!("POST {url}\n");

    let response = client
        .request_with_retry(reqwest::Method::POST, &url, Some(payload), None, true)
        .await
        .map_err(|e| anyhow::anyhow!("upstream request failed: {e}"))?;

    println!("HTTP {}\n", response.status());

    let byte_stream = response.bytes_stream();

    // Only allocate a capture buffer when `--capture` was requested, so a
    // plain probe run has no extra memory/CPU overhead.
    let captured = capture
        .is_some()
        .then(|| Arc::new(std::sync::Mutex::new(Vec::<u8>::new())));
    let sink = captured.clone();
    // Tee each raw chunk into the capture buffer (if any) as it flows through
    // the stream, without altering what's forwarded to the parser below.
    // The lock can only be poisoned if a previous holder panicked while
    // holding it, but the critical section here is just an infallible
    // `Vec::extend_from_slice`, so poisoning is unreachable in practice;
    // recovering via `into_inner` (rather than `.expect`) means we still
    // salvage whatever bytes were captured even if that invariant is ever
    // violated, instead of turning a benign panic into probe/replay failure.
    let byte_stream = byte_stream.inspect(move |chunk| {
        if let (Some(sink), Ok(bytes)) = (sink.as_ref(), chunk) {
            sink.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .extend_from_slice(bytes);
        }
    });

    let events = parse_kiro_stream(
        byte_stream,
        config.first_token_timeout,
        config.streaming_read_timeout,
    );

    let summary = print_events(events).await?;
    println!("\n{summary}");

    // Flush the captured raw bytes to disk only after the stream has fully
    // drained, so the fixture file reflects the complete response. See the
    // note above `byte_stream.inspect` on why `unwrap_or_else(into_inner)`
    // (rather than `.expect`) is used to read the lock here too.
    if let (Some(path), Some(sink)) = (capture, captured) {
        let bytes = sink
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        std::fs::write(&path, bytes.as_slice())
            .with_context(|| format!("failed to write {}", path.display()))?;
        println!("captured {} raw bytes to {}", bytes.len(), path.display());
    }

    Ok(())
}

/// Generates a fresh random conversation id for the `probe` subcommand by
/// delegating to `lanius_core`'s conversation-id generator with no seed
/// components.
fn uuid_v4() -> String {
    lanius_core::utils::generate_conversation_id(&[])
}
