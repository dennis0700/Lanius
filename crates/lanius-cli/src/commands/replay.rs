//! Implements the `replay <raw-stream-file>` subcommand: offline-decodes a
//! previously captured raw upstream byte stream (see `probe --capture`)
//! through the same SSE/event parser used for live traffic, useful for
//! debugging the parser without hitting the network.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use lanius_core::upstream::parse_kiro_stream;

use super::events::print_events;

/// Implements the `replay <raw-stream-file>` subcommand: reads a previously
/// captured raw upstream byte stream from disk and feeds it through the same
/// SSE/event parser ([`parse_kiro_stream`]) used for live traffic, entirely
/// offline (no network access).
///
/// Parameters:
/// - `path`: filesystem path to the raw capture file (as produced by
///   `probe --capture <file>`).
///
/// Side effects: reads the file from disk and prints decoded events plus a
/// summary to stdout.
///
/// Errors if the file cannot be read or if the stream fails to parse.
pub(crate) async fn run(path: &Path) -> Result<()> {
    let bytes =
        std::fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    println!("replaying {} ({} bytes)\n", path.display(), bytes.len());

    // Allow overriding the chunk size via env var so the same fixture can be
    // replayed with different byte-boundary framing (see the framing
    // independence tip in the module-level CLI docs) — this is how we prove
    // the parser is byte-boundary agnostic and doesn't rely on chunks lining
    // up with SSE event boundaries. Falls back to a fixed default (64) if
    // unset, invalid, or non-positive.
    let chunk_size: usize = std::env::var("REPLAY_CHUNK_SIZE")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|n| *n > 0)
        .unwrap_or(64);
    println!("chunk size: {chunk_size}\n");

    // Re-chunk the captured bytes into artificial "network reads" of
    // `chunk_size`, wrapped in `Ok` to mimic the `Result<Bytes, reqwest::Error>`
    // item type that a real `reqwest` byte stream would yield.
    let chunks: Vec<std::result::Result<bytes::Bytes, reqwest::Error>> = bytes
        .chunks(chunk_size)
        .map(|c| Ok(bytes::Bytes::copy_from_slice(c)))
        .collect();

    let stream = futures_util::stream::iter(chunks);
    let events = parse_kiro_stream(stream, Duration::from_secs(5), Duration::from_secs(5));

    let summary = print_events(events).await?;
    println!("\n{summary}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `replay` must successfully round-trip a tiny synthetic capture file
    /// through the parser without erroring, regardless of chunk size.
    #[tokio::test]
    async fn replay_runs_against_a_minimal_capture() {
        let dir = std::env::temp_dir().join(format!("lanius-replay-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("capture.bin");
        std::fs::write(&path, b"").expect("write empty fixture");

        let result = run(&path).await;
        assert!(result.is_ok(), "replay of an empty capture must not error");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A missing capture file must surface as an error rather than panicking.
    #[tokio::test]
    async fn replay_missing_file_is_an_error() {
        let path = std::env::temp_dir().join("lanius-replay-does-not-exist.bin");
        assert!(run(&path).await.is_err());
    }
}
