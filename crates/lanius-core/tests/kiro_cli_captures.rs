//! Golden tests over raw `GenerateAssistantResponse` streams captured from the
//! official Kiro CLI 3.5.0 (`KiroRuntimeService`), decoded with every chunk
//! size from one byte up so framing never affects the result.

use std::time::Duration;

use bytes::Bytes;
use lanius_core::upstream::{StreamResult, collect_stream_to_result};

const TOOL_USE: &[u8] = include_bytes!("fixtures/kiro_cli_3_5_tool_use.bin");
const REDACTED_REASONING: &[u8] = include_bytes!("fixtures/kiro_cli_3_5_redacted_reasoning.bin");

async fn collect(bytes: &[u8], chunk_size: usize) -> StreamResult {
    let chunks: Vec<Result<Bytes, reqwest::Error>> = bytes
        .chunks(chunk_size)
        .map(|chunk| Ok(Bytes::copy_from_slice(chunk)))
        .collect();
    collect_stream_to_result(
        futures_util::stream::iter(chunks),
        Duration::from_secs(5),
        Duration::from_secs(5),
    )
    .await
    .unwrap_or_else(|error| panic!("capture must decode: {error}"))
}

#[tokio::test]
async fn tool_use_turn_decodes_identically_for_every_chunk_size() {
    let whole = collect(TOOL_USE, TOOL_USE.len()).await;
    assert_eq!(whole.tool_calls.len(), 1);
    let call = &whole.tool_calls[0];
    assert_eq!(call.name, "execute_bash");
    assert_eq!(
        call.id.as_deref(),
        Some("toolu_bdrk_0152KwqDXEQ7aYxUXZbNCEZT")
    );
    let arguments: serde_json::Value =
        serde_json::from_str(&call.arguments).expect("arguments are JSON");
    assert_eq!(arguments["command"], "echo lanius-probe");
    assert!(call.truncation.is_none());
    assert!(
        whole
            .thinking_signature
            .as_deref()
            .is_some_and(|signature| signature.starts_with(".KTR~~")),
        "the hidden-reasoning signature must survive for replay"
    );
    assert!(
        whole.thinking_content.is_empty(),
        "the \"...\" placeholder is not thinking text"
    );
    assert_eq!(whole.stop_reason.as_deref(), Some("TOOL_USE"));
    assert!(whole.context_usage_percentage.is_some());
    assert!(whole.usage.is_some());
    assert!(whole.content.is_empty());

    for chunk_size in 1..=64 {
        assert_eq!(
            collect(TOOL_USE, chunk_size).await,
            whole,
            "chunk size {chunk_size}"
        );
    }
}

#[tokio::test]
async fn redacted_reasoning_turn_decodes_identically_for_every_chunk_size() {
    let whole = collect(REDACTED_REASONING, REDACTED_REASONING.len()).await;
    assert_eq!(whole.content, "pong");
    assert!(
        whole
            .redacted_thinking
            .as_deref()
            .is_some_and(|data| !data.is_empty()),
        "redacted reasoning must survive for replay"
    );
    assert!(whole.thinking_signature.is_none());
    assert_eq!(whole.stop_reason.as_deref(), Some("END_TURN"));
    assert!(whole.tool_calls.is_empty());

    for chunk_size in 1..=64 {
        assert_eq!(
            collect(REDACTED_REASONING, chunk_size).await,
            whole,
            "chunk size {chunk_size}"
        );
    }
}
