//! Shared "sink" for decoded [`KiroEvent`] streams, used by both the
//! `replay` and `probe` subcommands so they produce identically formatted
//! output and can be diffed against each other.

use anyhow::Result;
use futures_util::StreamExt;
use lanius_core::upstream::{KiroEvent, KiroEventType};

/// Running tally of event kinds seen while draining a [`KiroEvent`] stream,
/// printed as a one-line summary after all events have been processed.
#[derive(Default)]
pub(crate) struct Summary {
    content_events: usize,
    thinking_events: usize,
    tool_calls: usize,
    usage_events: usize,
    context_usage_events: usize,
    content_chars: usize,
}

impl std::fmt::Display for Summary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "summary: {} content ({} chars), {} thinking, {} tool_use, {} usage, {} context_usage",
            self.content_events,
            self.content_chars,
            self.thinking_events,
            self.tool_calls,
            self.usage_events,
            self.context_usage_events,
        )
    }
}

/// Drains a decoded [`KiroEvent`] stream (from either `replay` or `probe`),
/// printing a human-readable line per event to stdout and classifying each
/// event by [`KiroEventType`] into a running [`Summary`].
///
/// Returns the accumulated [`Summary`] once the stream ends, or an error if
/// any individual event in the stream is an `Err` (i.e. a parser/transport
/// failure, distinct from the in-band `KiroEventType::Error` event which is
/// merely printed).
pub(crate) async fn print_events(
    events: impl futures_util::Stream<Item = lanius_core::Result<KiroEvent>>,
) -> Result<Summary> {
    let mut events = std::pin::pin!(events);
    let mut summary = Summary::default();

    while let Some(event) = events.next().await {
        let event = event.map_err(|e| anyhow::anyhow!("stream error: {e}"))?;
        match event.event_type {
            KiroEventType::Content => {
                let c = event.content.unwrap_or_default();
                summary.content_events += 1;
                summary.content_chars += c.chars().count();
                println!("[content]       {c:?}");
            }
            KiroEventType::Thinking => {
                summary.thinking_events += 1;
                if let Some(c) = event.thinking_content {
                    println!("[thinking]      {c:?}");
                }
                if let Some(signature) = event.thinking_signature {
                    println!("[thinking sig]  {} chars", signature.chars().count());
                }
            }
            KiroEventType::ToolUse => {
                summary.tool_calls += 1;
                if let Some(tc) = event.tool_use {
                    println!(
                        "[tool_use]      id={:?} name={} args={}",
                        tc.id, tc.name, tc.arguments
                    );
                    if let Some(t) = tc.truncation {
                        println!(
                            "                !! TRUNCATED by upstream: {} ({} bytes)",
                            t.reason, t.size_bytes
                        );
                    }
                }
            }
            KiroEventType::Usage => {
                summary.usage_events += 1;
                println!("[usage]         {:?}", event.usage);
            }
            KiroEventType::ContextUsage => {
                summary.context_usage_events += 1;
                println!("[context_usage] {:?}", event.context_usage_percentage);
            }
            KiroEventType::Error => {
                println!("[error]         {:?}", event.content);
            }
        }
    }

    Ok(summary)
}
