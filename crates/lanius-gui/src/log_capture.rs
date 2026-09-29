//! In-memory capture of `tracing` log events for display in the GUI.
//!
//! Lanius GUI runs the embedded `lanius-core` gateway in-process (see
//! `server.rs`), so its log output is not a separate OS process's stdout
//! that could be piped — it's just `tracing` events emitted on whatever
//! thread/task produced them. [`CaptureLayer`] is a `tracing_subscriber`
//! [`Layer`] installed alongside the normal terminal formatter in
//! `main.rs`'s `main()`; it renders each event to a single stamped line and
//! appends it to a shared, bounded [`LogBuffer`]. `controller.rs` polls
//! [`ServerManager::get_logs`](crate::server::ServerManager::get_logs) (which
//! itself reads from a `LogBuffer`) on a timer and feeds the raw lines
//! through `logs::process` for display; `logs.rs` is responsible for
//! stripping ANSI and re-parsing the timestamp this module prepends.
//!
//! Because `tracing` events can be recorded from any thread (async tasks,
//! background workers, etc.), [`LogBuffer`] uses a `Mutex` internally and is
//! `Clone` (cheaply, via `Arc`) so every part of the app that wants to log
//! shares the same underlying buffer.

use std::sync::{Arc, Mutex};

use tracing::Level;
use tracing::field::{Field, Visit};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;

/// Maximum number of log lines retained in memory; older lines are evicted
/// once this cap is reached to bound memory use during long-running
/// sessions.
pub const MAX_LOG_LINES: usize = 2000;

/// A shared, thread-safe, bounded ring buffer of formatted log lines.
///
/// Cloning an instance shares the same underlying storage (via `Arc`), so
/// the tracing capture layer, the embedded gateway's own logger
/// (`server.rs`), and the polling code in `controller.rs` can all hold a
/// handle to the same buffer.
#[derive(Clone, Default)]
pub struct LogBuffer {
    lines: Arc<Mutex<Vec<String>>>,
}

impl LogBuffer {
    /// Creates an empty log buffer.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// let buffer = crate::log_capture::LogBuffer::new();
    /// assert!(buffer.get_all().is_empty());
    /// ```
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns a snapshot copy of every currently buffered line, oldest
    /// first. Recovers gracefully (rather than panicking) if the lock was
    /// poisoned by a prior panic, since losing log history is preferable to
    /// crashing the GUI.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// let buffer = crate::log_capture::LogBuffer::new();
    /// buffer.push_stamped("2026-01-01 12:00:00 | INFO started".to_string());
    /// assert_eq!(buffer.get_all().len(), 1);
    /// ```
    pub fn get_all(&self) -> Vec<String> {
        match self.lines.lock() {
            Ok(lines) => lines.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    /// Discards all buffered lines (used by the "Clear logs" UI action).
    ///
    /// # Examples
    ///
    /// ```ignore
    /// let buffer = crate::log_capture::LogBuffer::new();
    /// buffer.push_stamped("2026-01-01 12:00:00 | INFO started".to_string());
    /// buffer.clear();
    /// assert!(buffer.get_all().is_empty());
    /// ```
    pub fn clear(&self) {
        if let Ok(mut lines) = self.lines.lock() {
            lines.clear();
        }
    }

    /// Appends an already-formatted, already-timestamped line to the
    /// buffer, evicting the oldest lines first if the buffer is at
    /// [`MAX_LOG_LINES`] capacity.
    ///
    /// This is the primitive both [`CaptureLayer`] (for `tracing` events)
    /// and `server.rs` (for the embedded gateway's own status messages) use
    /// to write into the buffer, so both sources share one eviction policy.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// let buffer = crate::log_capture::LogBuffer::new();
    /// let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
    /// buffer.push_stamped(format!("{now} | Gateway started"));
    /// ```
    pub fn push_stamped(&self, stamped: String) {
        if let Ok(mut lines) = self.lines.lock() {
            if lines.len() >= MAX_LOG_LINES {
                let overflow = lines.len() + 1 - MAX_LOG_LINES;
                lines.drain(..overflow);
            }
            lines.push(stamped);
        }
    }

    /// Formats `message` with a local-time `YYYY-MM-DD HH:MM:SS` prefix and
    /// stores it via [`push_stamped`](Self::push_stamped). This timestamp
    /// format is what `logs::extract_time` expects to find and strip back
    /// out when rendering log rows in the UI.
    fn push(&self, message: &str) {
        let stamped = format!(
            "{} | {message}",
            chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
        );
        self.push_stamped(stamped);
    }
}

/// A `tracing::field::Visit` implementation that pulls the conventional
/// `message` field out of a tracing event separately from any other
/// structured fields, so the message can be placed first in the rendered
/// line and the rest appended as `key=value` pairs.
#[derive(Default)]
struct MessageVisitor {
    message: Option<String>,
    extra: Vec<(String, String)>,
}

impl Visit for MessageVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        let rendered = format!("{value:?}");
        if field.name() == "message" {
            self.message = Some(rendered);
        } else {
            self.extra.push((field.name().to_string(), rendered));
        }
    }
}

/// A `tracing_subscriber` layer that renders every event it observes into a
/// single line and appends it to a [`LogBuffer`].
///
/// Installed in `main.rs` alongside the normal terminal `fmt` layer, so
/// every `tracing::info!`/`warn!`/etc. call anywhere in the process (GUI
/// code and the embedded `lanius-core` gateway alike) is mirrored into this
/// buffer for display in the GUI's log view.
pub struct CaptureLayer {
    buffer: LogBuffer,
}

impl CaptureLayer {
    /// Creates a capture layer that appends rendered events to `buffer`.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// use tracing_subscriber::layer::SubscriberExt;
    /// use crate::log_capture::{CaptureLayer, LogBuffer};
    ///
    /// let buffer = LogBuffer::new();
    /// let subscriber = tracing_subscriber::registry().with(CaptureLayer::new(buffer.clone()));
    /// ```
    pub fn new(buffer: LogBuffer) -> Self {
        Self { buffer }
    }
}

impl<S> Layer<S> for CaptureLayer
where
    S: tracing::Subscriber,
{
    /// Renders a single tracing event as `LEVEL target: message key=value
    /// ...` and pushes it (with a timestamp prefix) into the buffer. This
    /// runs synchronously on whichever thread emitted the event, so it must
    /// stay cheap — it does no I/O beyond the in-memory buffer write.
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let mut visitor = MessageVisitor::default();
        event.record(&mut visitor);

        let level = match *event.metadata().level() {
            Level::ERROR => "ERROR",
            Level::WARN => "WARN",
            Level::INFO => "INFO",
            Level::DEBUG => "DEBUG",
            Level::TRACE => "TRACE",
        };

        let mut line = format!(
            "{level:>5} {}: {}",
            event.metadata().target(),
            visitor.message.unwrap_or_default()
        );
        for (key, value) in visitor.extra {
            line.push_str(&format!(" {key}={value}"));
        }

        self.buffer.push(&line);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing_subscriber::layer::SubscriberExt;

    #[test]
    fn events_from_any_target_are_captured_and_stamped() {
        let buffer = LogBuffer::new();
        let subscriber = tracing_subscriber::registry().with(CaptureLayer::new(buffer.clone()));

        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(count = 3, "hello from a library crate");
            tracing::warn!("a warning");
        });

        let lines = buffer.get_all();
        assert_eq!(lines.len(), 2, "both events must be captured: {lines:?}");
        assert!(
            lines[0].contains("INFO") && lines[0].contains("hello from a library crate"),
            "got {:?}",
            lines[0]
        );
        assert!(
            lines[0].contains("count=3"),
            "structured fields must be captured: {:?}",
            lines[0]
        );
        assert!(lines[1].contains("WARN") && lines[1].contains("a warning"));
        assert!(
            !crate::logs::extract_time(&lines[0]).is_empty(),
            "every captured line must carry a timestamp `extract_time` can parse: {:?}",
            lines[0]
        );
    }

    #[test]
    fn buffer_stays_bounded() {
        let buffer = LogBuffer::new();
        for i in 0..(MAX_LOG_LINES + 10) {
            buffer.push(&format!("line {i}"));
        }
        let lines = buffer.get_all();
        assert_eq!(lines.len(), MAX_LOG_LINES);
        assert!(
            lines
                .last()
                .unwrap()
                .ends_with(&format!("line {}", MAX_LOG_LINES + 9))
        );
    }
}
