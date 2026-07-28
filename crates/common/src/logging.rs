use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::Once;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tracing::Subscriber;
use tracing::field::{Field, Visit};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};

pub fn env_filter_from_default_env(default_directive: &str) -> EnvFilter {
    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_directive))
}

pub fn compact_fmt_layer<S>() -> impl Layer<S> + Send + Sync
where
    S: Subscriber + for<'span> LookupSpan<'span>,
{
    tracing_subscriber::fmt::layer()
        .with_timer(tracing_subscriber::fmt::time::SystemTime)
        .with_target(false)
        .compact()
}

pub fn init_compact_tracing(env_filter: EnvFilter) {
    static TRACING_INIT: Once = Once::new();
    TRACING_INIT.call_once(move || {
        let _ = tracing_subscriber::registry()
            .with(env_filter)
            .with(compact_fmt_layer())
            .try_init();
    });
}

pub fn init_compact_tracing_default(default_directive: &str) {
    init_compact_tracing(env_filter_from_default_env(default_directive));
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LogBufferEntry {
    pub captured_at_unix: u64,
    pub line: String,
}

pub struct LogBuffer {
    entries: StdMutex<VecDeque<LogBufferEntry>>,
    max_entries: usize,
}

impl LogBuffer {
    pub const DEFAULT_DIAGNOSTIC_CAPACITY: usize = 2_000;
    pub const MOBILE_DIAGNOSTIC_CAPACITY: usize = 20_000;

    pub fn new(max_entries: usize) -> Self {
        Self {
            entries: StdMutex::new(VecDeque::with_capacity(max_entries.max(1))),
            max_entries: max_entries.max(1),
        }
    }

    pub fn push(&self, line: impl Into<String>) {
        self.push_with_timestamp(unix_ts(), line.into());
    }

    pub fn push_with_timestamp(&self, captured_at_unix: u64, line: String) {
        let mut entries = match self.entries.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };

        entries.push_back(LogBufferEntry {
            captured_at_unix,
            line,
        });
        while entries.len() > self.max_entries {
            entries.pop_front();
        }
    }

    pub fn recent(&self, limit: usize) -> Vec<LogBufferEntry> {
        let entries = match self.entries.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };

        let keep = limit.max(1);
        let skip = entries.len().saturating_sub(keep);
        entries.iter().skip(skip).cloned().collect()
    }

    /// Returns every retained entry captured at or after `earliest_unix`.
    ///
    /// This intentionally does not apply an entry-count limit. Consumers that
    /// export a diagnostic time window should receive the complete retained
    /// context for that window, subject only to the buffer's bounded capacity.
    pub fn recent_since(&self, earliest_unix: u64) -> Vec<LogBufferEntry> {
        let entries = match self.entries.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };

        entries
            .iter()
            .filter(|entry| entry.captured_at_unix >= earliest_unix)
            .cloned()
            .collect()
    }

    /// Renders every retained entry as plain text in capture order.
    pub fn render_text(&self) -> String {
        let entries = match self.entries.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let mut rendered = String::new();
        for entry in entries.iter() {
            let timestamp = OffsetDateTime::from_unix_timestamp(entry.captured_at_unix as i64)
                .ok()
                .and_then(|value| value.format(&Rfc3339).ok())
                .unwrap_or_else(|| format!("unix:{}", entry.captured_at_unix));
            rendered.push_str(&timestamp);
            rendered.push(' ');
            rendered.push_str(entry.line.trim_end());
            rendered.push('\n');
        }
        rendered
    }
}

#[derive(Clone)]
pub struct LogCaptureLayer {
    buffer: Arc<LogBuffer>,
}

impl LogCaptureLayer {
    pub fn new(buffer: Arc<LogBuffer>) -> Self {
        Self { buffer }
    }
}

struct EventFieldVisitor {
    fields: Vec<String>,
}

impl EventFieldVisitor {
    fn new() -> Self {
        Self { fields: Vec::new() }
    }
}

impl Visit for EventFieldVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.fields.push(format!("{}={:?}", field.name(), value));
    }
}

impl<S> Layer<S> for LogCaptureLayer
where
    S: Subscriber,
{
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut visitor = EventFieldVisitor::new();
        event.record(&mut visitor);
        let metadata = event.metadata();

        let line = if visitor.fields.is_empty() {
            format!("{} {}", metadata.level(), metadata.target())
        } else {
            format!(
                "{} {} {}",
                metadata.level(),
                metadata.target(),
                visitor.fields.join(" ")
            )
        };

        self.buffer.push(line);
    }
}

fn unix_ts() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::LogBuffer;

    #[test]
    fn recent_since_returns_all_entries_in_the_requested_time_window() {
        let buffer = LogBuffer::new(8);
        buffer.push_with_timestamp(100, "before window".to_string());
        buffer.push_with_timestamp(180, "at window start".to_string());
        buffer.push_with_timestamp(240, "inside window".to_string());

        let entries = buffer.recent_since(180);

        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].line, "at window start");
        assert_eq!(entries[1].line, "inside window");
    }

    #[test]
    fn diagnostic_capacity_keeps_a_larger_bounded_context() {
        let buffer = LogBuffer::new(LogBuffer::DEFAULT_DIAGNOSTIC_CAPACITY);
        for index in 0..=LogBuffer::DEFAULT_DIAGNOSTIC_CAPACITY {
            buffer.push_with_timestamp(index as u64, index.to_string());
        }

        let entries = buffer.recent(LogBuffer::DEFAULT_DIAGNOSTIC_CAPACITY + 1);

        assert_eq!(entries.len(), LogBuffer::DEFAULT_DIAGNOSTIC_CAPACITY);
        assert_eq!(entries.first().map(|entry| entry.line.as_str()), Some("1"));
        assert_eq!(
            entries.last().map(|entry| entry.line.as_str()),
            Some("2000")
        );
    }

    #[test]
    fn render_text_preserves_capture_order_and_normalizes_line_endings() {
        let buffer = LogBuffer::new(4);
        buffer.push_with_timestamp(100, "first\n".to_string());
        buffer.push_with_timestamp(101, "second".to_string());

        assert_eq!(
            buffer.render_text(),
            "1970-01-01T00:01:40Z first\n1970-01-01T00:01:41Z second\n"
        );
    }
}
