//! `-v`/`-vv`: timestamped diagnostics of this client and of the daemon's work
//! for its requests, on stderr only, then a timing summary. stdout keeps the
//! selected output mode untouched, so `--output json -v` stays parseable.

use std::{
    fmt::Write as _,
    io::Write as _,
    sync::{Mutex, OnceLock},
    time::Instant,
};

use devlanes_proto::Event;
use serde_json::Value;
use tracing::{Level, Subscriber, field::Field};
use tracing_subscriber::{
    Layer,
    filter::Targets,
    fmt::{format::Writer, time::FormatTime},
    layer::{Context, SubscriberExt},
    registry::LookupSpan,
    util::SubscriberInitExt,
};

/// When this process started counting, shared by every diagnostic line.
fn start() -> Instant {
    static START: OnceLock<Instant> = OnceLock::new();
    *START.get_or_init(Instant::now)
}

/// `(label, milliseconds)` in the order they were measured.
fn timings() -> &'static Mutex<Vec<(String, f64)>> {
    static TIMINGS: OnceLock<Mutex<Vec<(String, f64)>>> = OnceLock::new();
    TIMINGS.get_or_init(Mutex::default)
}

/// Adds one row to the timing summary.
fn record(label: String, ms: f64) {
    if let Ok(mut rows) = timings().lock() {
        rows.push((label, ms));
    }
}

/// Seconds since start, like every diagnostic line begins.
struct Since;

impl FormatTime for Since {
    /// `   1.234s`.
    fn format_time(&self, w: &mut Writer<'_>) -> std::fmt::Result {
        write!(w, "{:>8.3}s", start().elapsed().as_secs_f64())
    }
}

/// Starts client diagnostics at `level` (1 debug, 2 trace) when it is above 0:
/// a stderr subscriber for devlanes targets and the daemon diagnostics sink.
pub fn init(level: u8) {
    if level == 0 {
        return;
    }
    start();
    let wanted = if level >= 2 {
        Level::TRACE
    } else {
        Level::DEBUG
    };
    let targets = Targets::new()
        .with_target("devlanes", wanted)
        .with_default(Level::WARN);
    let fmt = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_timer(Since)
        .with_ansi(false)
        .compact()
        .with_filter(targets.clone());
    let _ = tracing_subscriber::registry()
        .with(fmt)
        .with(TimingLayer.with_filter(targets))
        .try_init();
    devlanes_client::diagnostics::enable(level, move |event| daemon_line(event, level));
}

/// Prints one daemon diagnostic in the client's line shape and keeps span
/// durations for the summary. Span closes print at `-vv` only.
fn daemon_line(event: &Event, level: u8) {
    let data = &event.data;
    let text = |key: &str| data.get(key).and_then(Value::as_str).unwrap_or_default();
    let elapsed = data
        .get("elapsed_ms")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let mut line = format!("{:>8.3}s ", start().elapsed().as_secs_f64());
    match text("kind") {
        "span" => {
            let duration = data
                .get("duration_ms")
                .and_then(Value::as_f64)
                .unwrap_or(0.0);
            let name = text("name");
            record(
                format!("daemon {}", span_label(name, &data["fields"])),
                duration,
            );
            if level < 2 {
                return;
            }
            let _ = write!(
                line,
                " SPAN daemon +{elapsed:.0}ms {}: closed after {duration:.1}ms",
                span_label(name, &data["fields"])
            );
        }
        "dropped" => {
            let _ = write!(
                line,
                " WARN daemon: {} diagnostic records were dropped (the client read too slowly)",
                data.get("count").and_then(Value::as_u64).unwrap_or(0)
            );
        }
        _ => {
            let spans: Vec<&str> = data
                .get("spans")
                .and_then(Value::as_array)
                .map(|spans| spans.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            let _ = write!(
                line,
                "{:>5} daemon +{elapsed:.0}ms {}: ",
                text("level").to_uppercase(),
                text("target")
            );
            if !spans.is_empty() {
                let _ = write!(line, "{}: ", spans.join(":"));
            }
            line.push_str(text("message"));
            if let Some(fields) = data.get("fields").and_then(Value::as_object) {
                for (key, value) in fields {
                    let _ = write!(line, " {key}={}", plain(value));
                }
            }
        }
    }
    line.push('\n');
    let _ = std::io::stderr().lock().write_all(line.as_bytes());
}

/// `phase infra`, `unit api`, `request start`: a span by its identifying field.
fn span_label(name: &str, fields: &Value) -> String {
    let key = ["name", "unit", "method", "container"]
        .into_iter()
        .find_map(|key| fields.get(key).map(plain));
    match key {
        Some(key) => format!("{name} {key}"),
        None => name.to_owned(),
    }
}

/// A JSON value without quotes around strings.
fn plain(value: &Value) -> String {
    value
        .as_str()
        .map_or_else(|| value.to_string(), str::to_owned)
}

/// Ends a verbose run: the exit code with its source, then the timing summary.
pub fn finish(level: u8, code: u8) {
    if level == 0 {
        return;
    }
    tracing::debug!(
        exit_code = code,
        total_ms = start().elapsed().as_millis() as u64,
        "exit"
    );
    let rows = timings()
        .lock()
        .map(|rows| rows.clone())
        .unwrap_or_default();
    let mut text = String::from("timings:\n");
    let width = rows.iter().map(|(label, _)| label.len()).max().unwrap_or(0);
    for (label, ms) in &rows {
        let _ = writeln!(text, "  {label:<width$}  {}", duration(*ms));
    }
    let _ = writeln!(
        text,
        "  {:<width$}  {}",
        "total",
        duration(start().elapsed().as_secs_f64() * 1000.0)
    );
    let _ = std::io::stderr().lock().write_all(text.as_bytes());
}

/// `12ms` below a second, `1.83s` above.
fn duration(ms: f64) -> String {
    if ms < 1000.0 {
        format!("{ms:.0}ms")
    } else {
        format!("{:.2}s", ms / 1000.0)
    }
}

/// Collects client events that carry `timing` (a label) and `elapsed_ms`.
struct TimingLayer;

impl<S> Layer<S> for TimingLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    /// Records the event's measurement when it has one.
    fn on_event(&self, event: &tracing::Event<'_>, _: Context<'_, S>) {
        let mut visitor = Timing::default();
        event.record(&mut visitor);
        if let (Some(label), Some(ms)) = (visitor.label, visitor.ms) {
            record(label, ms);
        }
    }
}

/// The `timing` and `elapsed_ms` fields of one event.
#[derive(Default)]
struct Timing {
    label: Option<String>,
    ms: Option<f64>,
}

impl tracing::field::Visit for Timing {
    /// `elapsed_ms` as a number.
    fn record_u64(&mut self, field: &Field, value: u64) {
        if field.name() == "elapsed_ms" {
            self.ms = Some(value as f64);
        }
    }

    /// `timing` given as a string literal.
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "timing" {
            self.label = Some(value.to_owned());
        }
    }

    /// `timing` given with `%`.
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "timing" {
            self.label = Some(format!("{value:?}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    /// Spans are named by the field that tells them apart.
    fn span_labels() {
        assert_eq!(
            span_label("phase", &json!({"name": "infra", "lane": "acme@0"})),
            "phase infra"
        );
        assert_eq!(
            span_label("request", &json!({"method": "start", "id": 1})),
            "request start"
        );
        assert_eq!(span_label("readiness", &json!({})), "readiness");
        assert_eq!(duration(12.4), "12ms");
        assert_eq!(duration(1834.0), "1.83s");
    }
}
