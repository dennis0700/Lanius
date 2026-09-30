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

use std::fmt::Write as _;
use std::sync::{Arc, Mutex, OnceLock};

use tracing::Level;
use tracing::field::{Field, Visit};
use tracing_subscriber::layer::Context;
use tracing_subscriber::{EnvFilter, Layer, Registry, reload};

/// Maximum number of log lines retained in memory; older lines are evicted
/// once this cap is reached to bound memory use during long-running
/// sessions.
pub const MAX_LOG_LINES: usize = 2000;

/// Crates whose DEBUG/TRACE output is pure transport noise; they are capped
/// at INFO even when the user selects a more verbose level.
const NOISY_TARGETS: &[&str] = &["hyper", "hyper_util", "h2", "rustls", "reqwest", "tower"];

type FilterHandle = reload::Handle<EnvFilter, Registry>;

static FILTER_HANDLE: OnceLock<FilterHandle> = OnceLock::new();

/// Builds the global, runtime-reloadable log filter layer and remembers its
/// handle for [`apply_log_level`]. Starts from a valid `RUST_LOG` when set,
/// else `info`. Must be the first layer added to the `Registry` in
/// `main.rs`, and called only once.
///
/// # Examples
///
/// ```ignore
/// use tracing_subscriber::layer::SubscriberExt;
/// let subscriber = tracing_subscriber::registry().with(crate::log_capture::reloadable_filter());
/// ```
pub fn reloadable_filter() -> reload::Layer<EnvFilter, Registry> {
    let initial = env_filter_override().unwrap_or_else(|| filter_for_level("info"));
    let (layer, handle) = reload::Layer::new(initial);
    let newly_set = FILTER_HANDLE.set(handle).is_ok();
    debug_assert!(newly_set, "reloadable_filter must be called only once");
    layer
}

/// Applies the user-selected log level (e.g. `"INFO"`, `"debug"`) to every
/// log sink. A valid `RUST_LOG` environment override always wins, so
/// developers can still get targeted output regardless of the saved
/// setting; an invalid one is ignored.
///
/// # Examples
///
/// ```ignore
/// crate::log_capture::apply_log_level("DEBUG");
/// ```
pub fn apply_log_level(level: &str) {
    if env_filter_override().is_some() {
        return;
    }
    let Some(handle) = FILTER_HANDLE.get() else {
        return;
    };
    if let Err(error) = handle.reload(filter_for_level(level)) {
        tracing::warn!(%error, "failed to apply log level");
    }
}

/// The filter from `RUST_LOG`, if it is set and parses.
fn env_filter_override() -> Option<EnvFilter> {
    EnvFilter::try_from_default_env().ok()
}

/// Builds the filter for a UI log level, falling back to `info` for
/// unrecognized values and capping [`NOISY_TARGETS`] at `info`.
fn filter_for_level(level: &str) -> EnvFilter {
    let lowered = level.trim().to_ascii_lowercase();
    let level = match lowered.as_str() {
        l @ ("error" | "warn" | "info" | "debug" | "trace") => l,
        "warning" => "warn",
        _ => "info",
    };
    let mut directives = String::from(level);
    if matches!(level, "debug" | "trace") {
        for target in NOISY_TARGETS {
            let _ = write!(directives, ",{target}=info");
        }
    }
    EnvFilter::try_new(&directives).unwrap_or_else(|_| EnvFilter::new("info"))
}

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

/// Rendered `key=value` fields of a span, stored in its extensions when the
/// span is created so [`CaptureLayer::on_event`] can prefix events with the
/// context of the request/operation they belong to (e.g. tower-http's
/// `request{method=POST uri=/v1/messages}`).
struct SpanFields(String);

#[derive(Default)]
struct FieldsVisitor(String);

impl FieldsVisitor {
    fn separator(&mut self) {
        if !self.0.is_empty() {
            self.0.push(' ');
        }
    }
}

impl Visit for FieldsVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.separator();
        let _ = write!(self.0, "{}={value:?}", field.name());
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.separator();
        let _ = write!(self.0, "{}={value}", field.name());
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
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        id: &tracing::span::Id,
        ctx: Context<'_, S>,
    ) {
        let Some(span) = ctx.span(id) else {
            return;
        };
        let mut visitor = FieldsVisitor::default();
        attrs.record(&mut visitor);
        span.extensions_mut().insert(SpanFields(visitor.0));
    }

    /// Appends fields recorded after span creation (`span.record(..)`).
    fn on_record(
        &self,
        id: &tracing::span::Id,
        values: &tracing::span::Record<'_>,
        ctx: Context<'_, S>,
    ) {
        let Some(span) = ctx.span(id) else {
            return;
        };
        let mut extensions = span.extensions_mut();
        let existing = extensions
            .get_mut::<SpanFields>()
            .map(|fields| std::mem::take(&mut fields.0))
            .unwrap_or_default();
        let mut visitor = FieldsVisitor(existing);
        values.record(&mut visitor);
        match extensions.get_mut::<SpanFields>() {
            Some(fields) => fields.0 = visitor.0,
            None => extensions.insert(SpanFields(visitor.0)),
        }
    }

    /// Renders a single tracing event as `LEVEL target: [span{fields}]
    /// message key=value ...` and pushes it (with a timestamp prefix) into
    /// the buffer. This runs synchronously on whichever thread emitted the
    /// event, so it must stay cheap — it does no I/O beyond the in-memory
    /// buffer write.
    fn on_event(&self, event: &tracing::Event<'_>, ctx: Context<'_, S>) {
        let mut visitor = MessageVisitor::default();
        event.record(&mut visitor);

        let level = match *event.metadata().level() {
            Level::ERROR => "ERROR",
            Level::WARN => "WARN",
            Level::INFO => "INFO",
            Level::DEBUG => "DEBUG",
            Level::TRACE => "TRACE",
        };

        let mut line = format!("{level:>5} {}: ", event.metadata().target());
        if let Some(scope) = ctx.event_scope(event) {
            for span in scope.from_root() {
                let extensions = span.extensions();
                let _ = match extensions.get::<SpanFields>() {
                    Some(fields) if !fields.0.is_empty() => {
                        write!(line, "{}{{{}}} ", span.name(), fields.0)
                    }
                    _ => write!(line, "{} ", span.name()),
                };
            }
        }
        line.push_str(&visitor.message.unwrap_or_default());
        for (key, value) in visitor.extra {
            let _ = write!(line, " {key}={value}");
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
    fn span_fields_prefix_captured_events() {
        let buffer = LogBuffer::new();
        let subscriber = tracing_subscriber::registry().with(CaptureLayer::new(buffer.clone()));

        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!("request", method = "POST", uri = "/v1/messages");
            let _entered = span.enter();
            tracing::warn!(status = 401, "POST /v1/messages rejected");
        });

        let lines = buffer.get_all();
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(
            lines[0].contains("request{method=POST uri=/v1/messages} POST /v1/messages rejected"),
            "span context must be rendered: {:?}",
            lines[0]
        );
        assert!(lines[0].contains("status=401"), "{:?}", lines[0]);
    }

    #[test]
    fn fields_recorded_after_span_creation_are_captured() {
        let buffer = LogBuffer::new();
        let subscriber = tracing_subscriber::registry().with(CaptureLayer::new(buffer.clone()));

        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!("op", id = 7, stage = tracing::field::Empty);
            span.record("stage", "late");
            let _entered = span.enter();
            tracing::info!("done");
        });

        let lines = buffer.get_all();
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(
            lines[0].contains("op{id=7 stage=late} done"),
            "{:?}",
            lines[0]
        );
    }

    #[test]
    fn log_level_filters_accept_ui_values() {
        for level in [
            "ERROR", "warn", "Warning", "INFO", "debug", "TRACE", "bogus", "",
        ] {
            let rendered = filter_for_level(level).to_string();
            assert!(!rendered.is_empty(), "{level:?} produced an empty filter");
        }
        let debug = filter_for_level("DEBUG").to_string();
        assert!(debug.contains("debug"), "{debug}");
        assert!(
            debug.contains("hyper=info"),
            "noisy crates stay capped: {debug}"
        );
        assert!(filter_for_level("bogus").to_string().contains("info"));
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
