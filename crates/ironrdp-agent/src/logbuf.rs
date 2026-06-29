//! Bounded, queryable in-memory log buffer and the [`tracing`] layer that fills it.
//!
//! The daemon installs [`LogLayer`] as a global tracing layer so that the logs emitted while
//! driving the RDP client are retained in a small ring buffer for on-demand inspection via
//! `Request::QueryLogs`, rather than printed to the terminal. The buffer scrubs any registered
//! secret value before retaining a line.

use core::fmt::Write as _;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use ironrdp_cfg::is_secret_key;
use ironrdp_propertyset::{PropertySet, Value};
use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;

/// Default ring-buffer capacity, in lines.
const DEFAULT_CAPACITY: usize = 100;

/// A bounded ring buffer of formatted log lines, with secret-value scrubbing.
pub(crate) struct LogBuffer {
    inner: Mutex<Inner>,
}

struct Inner {
    capacity: usize,
    lines: VecDeque<String>,
    secrets: Vec<String>,
}

impl LogBuffer {
    pub(crate) fn new() -> Arc<Self> {
        Self::with_capacity(DEFAULT_CAPACITY)
    }

    pub(crate) fn with_capacity(capacity: usize) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(Inner {
                capacity: capacity.max(1),
                lines: VecDeque::new(),
                secrets: Vec::new(),
            }),
        })
    }

    /// Registers every secret value found in `properties` so it is scrubbed from future log lines.
    pub(crate) fn register_secrets(&self, properties: &PropertySet) {
        let mut inner = self.inner.lock().expect("log buffer poisoned");
        for (key, value) in properties.iter() {
            let Value::Str(secret) = value else {
                continue;
            };
            if is_secret_key(key) && !secret.is_empty() && !inner.secrets.iter().any(|known| known == secret) {
                inner.secrets.push(secret.clone());
            }
        }
    }

    fn push(&self, line: String) {
        let mut inner = self.inner.lock().expect("log buffer poisoned");

        // Scrub any registered secret value before retaining the line.
        let mut line = line;
        for secret in &inner.secrets {
            if line.contains(secret.as_str()) {
                line = line.replace(secret.as_str(), crate::ipc::REDACTED);
            }
        }

        if inner.capacity <= inner.lines.len() {
            inner.lines.pop_front();
        }
        inner.lines.push_back(line);
    }

    /// Returns retained lines, optionally filtered to those containing `substring`.
    pub(crate) fn query(&self, substring: Option<&str>) -> Vec<String> {
        let inner = self.inner.lock().expect("log buffer poisoned");
        inner
            .lines
            .iter()
            .filter(|line| substring.is_none_or(|needle| line.contains(needle)))
            .cloned()
            .collect()
    }
}

/// Installs the ring-buffer layer as the global tracing subscriber (best-effort).
///
/// No formatting layer is attached, so the daemon stays quiet on the terminal; everything that
/// passes the `IRONRDP_LOG` filter (default `debug`) is retained in `buffer` instead.
pub(crate) fn install(buffer: Arc<LogBuffer>) {
    use tracing::level_filters::LevelFilter;
    use tracing_subscriber::EnvFilter;
    use tracing_subscriber::prelude::*;

    let env_filter = EnvFilter::builder()
        .with_default_directive(LevelFilter::DEBUG.into())
        .with_env_var("IRONRDP_LOG")
        .from_env_lossy();

    let _ = tracing_subscriber::registry()
        .with(env_filter)
        .with(LogLayer::new(buffer))
        .try_init();
}

/// A tracing [`Layer`] that formats each event into a single line and pushes it to a [`LogBuffer`].
struct LogLayer {
    buffer: Arc<LogBuffer>,
}

impl LogLayer {
    fn new(buffer: Arc<LogBuffer>) -> Self {
        Self { buffer }
    }
}

impl<S: Subscriber> Layer<S> for LogLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let meta = event.metadata();

        let mut visitor = LogVisitor {
            message: None,
            fields: String::new(),
        };
        event.record(&mut visitor);

        let mut line = String::new();
        let _ = write!(line, "{:>5} {}", meta.level(), meta.target());
        if let Some(message) = &visitor.message {
            let _ = write!(line, " {message}");
        }
        line.push_str(&visitor.fields);

        self.buffer.push(line);
    }
}

/// Collects an event's message and structured fields into strings.
struct LogVisitor {
    message: Option<String>,
    fields: String,
}

impl Visit for LogVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn core::fmt::Debug) {
        if field.name() == "message" {
            self.message = Some(format!("{value:?}"));
        } else {
            let _ = write!(self.fields, " {}={:?}", field.name(), value);
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = Some(value.to_owned());
        } else {
            let _ = write!(self.fields, " {}={}", field.name(), value);
        }
    }
}
