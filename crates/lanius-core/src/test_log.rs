//! Test-only capture of `tracing` events, so tests can assert on what the
//! gateway logged and at which level.

use std::fmt::Write as _;
use std::sync::{Arc, Mutex};

use tracing::field::{Field, Visit};
use tracing_subscriber::layer::{Context, SubscriberExt};

/// Captured events rendered as `LEVEL field=value ...`, in emission order.
#[derive(Clone, Default)]
pub(crate) struct CapturedLogs(Arc<Mutex<Vec<String>>>);

impl CapturedLogs {
    /// Installs a capturing subscriber as the thread default until the
    /// returned guard is dropped. Use with current-thread test runtimes.
    pub(crate) fn install() -> (Self, tracing::subscriber::DefaultGuard) {
        let logs = Self::default();
        let subscriber = tracing_subscriber::registry().with(logs.clone());
        (logs, tracing::subscriber::set_default(subscriber))
    }

    /// Snapshot of every captured line.
    pub(crate) fn lines(&self) -> Vec<String> {
        self.0.lock().map(|lines| lines.clone()).unwrap_or_default()
    }

    /// Whether some line has `level` and contains every one of `needles`.
    pub(crate) fn has(&self, level: &str, needles: &[&str]) -> bool {
        self.lines().iter().any(|line| {
            line.starts_with(level) && needles.iter().all(|needle| line.contains(needle))
        })
    }
}

struct LineVisitor(String);

impl Visit for LineVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        let _ = write!(self.0, " {}={value:?}", field.name());
    }
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for CapturedLogs {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let mut visitor = LineVisitor(event.metadata().level().to_string());
        event.record(&mut visitor);
        if let Ok(mut lines) = self.0.lock() {
            lines.push(visitor.0);
        }
    }
}
