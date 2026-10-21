//! Streams the daemon's tracing records of one request back to its client.
//!
//! A request whose [`devlanes_proto::Context::verbose`] is set gets a tap: the
//! request span carries the tap's id, and [`layer`] sends every record and span
//! close below that span, at the tap's level, to the tap's channel. The request
//! loop forwards them as [`devlanes_proto::events::DIAGNOSTIC`] events before the
//! response. Without an active tap the layer's filter rejects everything, so
//! debug and trace records cost one atomic load.

use std::{
    collections::HashMap,
    fmt::Write as _,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicU8, AtomicU64, AtomicUsize, Ordering},
    },
    time::Instant,
};

use serde_json::{Map, Value, json};
use tokio::sync::mpsc;
use tracing::{Level, Metadata, Subscriber, field::Field, span};
use tracing_subscriber::{
    Layer,
    layer::{Context, Filter},
    registry::LookupSpan,
};

/// Records a slow client may leave queued before newer ones are dropped.
const QUEUE: usize = 4096;

/// Field names whose values are never forwarded, whatever the call site logged.
const REDACTED: [&str; 4] = ["secret", "password", "token", "value"];

/// One request's subscription.
struct Tap {
    sender: mpsc::Sender<Value>,
    level: Level,
    started: Instant,
    dropped: AtomicU64,
}

/// Active taps by id, and the most verbose level among them.
#[derive(Default)]
struct Taps {
    by_id: Mutex<HashMap<u64, Arc<Tap>>>,
    active: AtomicUsize,
    /// 0 none, 1 debug, 2 trace.
    max: AtomicU8,
    next: AtomicU64,
}

/// The process-wide tap table.
fn taps() -> &'static Taps {
    static TAPS: OnceLock<Taps> = OnceLock::new();
    TAPS.get_or_init(Taps::default)
}

/// Locks the tap table; a panic elsewhere cannot poison what is only a map.
fn lock(taps: &Taps) -> std::sync::MutexGuard<'_, HashMap<u64, Arc<Tap>>> {
    taps.by_id
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Recomputes [`Taps::max`] after a change.
fn refresh(taps: &Taps, by_id: &HashMap<u64, Arc<Tap>>) {
    let max = by_id
        .values()
        .map(|tap| if tap.level == Level::TRACE { 2 } else { 1 })
        .max()
        .unwrap_or(0);
    taps.max.store(max, Ordering::SeqCst);
    taps.active.store(by_id.len(), Ordering::SeqCst);
}

/// Keeps a tap registered until dropped or [`TapGuard::finish`]ed.
pub struct TapGuard {
    id: u64,
    tap: Arc<Tap>,
}

impl TapGuard {
    /// The id to put in the request span's `tap` field.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Unregisters the tap and returns how many records were dropped. The
    /// channel closes once in-flight sends finish.
    pub fn finish(self) -> u64 {
        self.tap.dropped.load(Ordering::SeqCst)
    }
}

impl Drop for TapGuard {
    /// Removes the tap, so its sender goes and the receiver drains to the end.
    fn drop(&mut self) {
        let taps = taps();
        let mut by_id = lock(taps);
        by_id.remove(&self.id);
        refresh(taps, &by_id);
    }
}

/// Registers a tap for `verbose` (1 debug, 2 or more trace). Register before
/// creating the request span: the layer's filter only lets spans through while
/// a tap is active.
pub fn register(verbose: u8) -> (TapGuard, mpsc::Receiver<Value>) {
    let (sender, receiver) = mpsc::channel(QUEUE);
    let taps = taps();
    let id = taps.next.fetch_add(1, Ordering::Relaxed) + 1;
    let tap = Arc::new(Tap {
        sender,
        level: if verbose >= 2 {
            Level::TRACE
        } else {
            Level::DEBUG
        },
        started: Instant::now(),
        dropped: AtomicU64::new(0),
    });
    let mut by_id = lock(taps);
    by_id.insert(id, tap.clone());
    refresh(taps, &by_id);
    (TapGuard { id, tap }, receiver)
}

/// The layer to add to the daemon's subscriber, with its own filter.
pub fn layer<S>() -> impl Layer<S>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    TapLayer.with_filter(TapFilter)
}

/// Lets devlanes records through while a tap wants their level.
struct TapFilter;

impl<S> Filter<S> for TapFilter {
    /// Cheap: an atomic load, then a level and target comparison.
    fn enabled(&self, meta: &Metadata<'_>, _: &Context<'_, S>) -> bool {
        let max = match taps().max.load(Ordering::Relaxed) {
            0 => return false,
            1 => Level::DEBUG,
            _ => Level::TRACE,
        };
        *meta.level() <= max && meta.target().starts_with("devlanes")
    }

    /// `sometimes`, so taps registered later still see these call sites.
    fn callsite_enabled(&self, meta: &'static Metadata<'static>) -> tracing::subscriber::Interest {
        if meta.target().starts_with("devlanes") {
            tracing::subscriber::Interest::sometimes()
        } else {
            tracing::subscriber::Interest::never()
        }
    }

    /// Any level may be wanted by a future tap.
    fn max_level_hint(&self) -> Option<tracing::level_filters::LevelFilter> {
        Some(tracing::level_filters::LevelFilter::TRACE)
    }
}

/// Formats records and span closes for the tap above them.
struct TapLayer;

/// The tap id a span carries, set from its `tap` field.
struct TapId(u64);

/// A span's fields and its opening time.
struct SpanInfo {
    fields: Map<String, Value>,
    opened: Instant,
}

impl<S> Layer<S> for TapLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    /// Stores the span's fields and, for a request span, its tap.
    fn on_new_span(&self, attrs: &span::Attributes<'_>, id: &span::Id, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(id) else { return };
        let mut visitor = Fields::default();
        attrs.record(&mut visitor);
        let mut extensions = span.extensions_mut();
        if let Some(tap) = visitor.tap {
            extensions.insert(TapId(tap));
        }
        extensions.insert(SpanInfo {
            fields: visitor.fields,
            opened: Instant::now(),
        });
    }

    /// Picks up fields recorded after creation, `tap` included.
    fn on_record(&self, id: &span::Id, values: &span::Record<'_>, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(id) else { return };
        let mut visitor = Fields::default();
        values.record(&mut visitor);
        let mut extensions = span.extensions_mut();
        if let Some(tap) = visitor.tap {
            extensions.insert(TapId(tap));
        }
        if let Some(info) = extensions.get_mut::<SpanInfo>() {
            info.fields.extend(visitor.fields);
        }
    }

    /// Sends the record to the tap of the innermost request span above it.
    fn on_event(&self, event: &tracing::Event<'_>, ctx: Context<'_, S>) {
        let Some(scope) = ctx.event_scope(event) else {
            return;
        };
        let spans: Vec<_> = scope.from_root().collect();
        let Some(tap) = spans
            .iter()
            .rev()
            .find_map(|span| span.extensions().get::<TapId>().map(|id| id.0))
            .and_then(|id| lock(taps()).get(&id).cloned())
        else {
            return;
        };
        if *event.metadata().level() > tap.level {
            return;
        }
        let mut visitor = Fields::default();
        event.record(&mut visitor);
        let labels: Vec<String> = spans
            .iter()
            .map(|span| label(span.name(), &fields_of(span)))
            .collect();
        let record = json!({
            "level": event.metadata().level().as_str().to_lowercase(),
            "target": event.metadata().target(),
            "message": visitor.message,
            "fields": visitor.fields,
            "spans": labels,
            "elapsed_ms": elapsed_ms(tap.started),
        });
        send(&tap, record);
    }

    /// Reports how long a span below a tap was open.
    fn on_close(&self, id: span::Id, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(&id) else { return };
        let chain: Vec<_> = span.scope().from_root().collect();
        let Some(tap) = chain
            .iter()
            .rev()
            .find_map(|span| span.extensions().get::<TapId>().map(|id| id.0))
            .and_then(|id| lock(taps()).get(&id).cloned())
        else {
            return;
        };
        let extensions = span.extensions();
        let Some(info) = extensions.get::<SpanInfo>() else {
            return;
        };
        let parents: Vec<String> = chain
            .iter()
            .take(chain.len().saturating_sub(1))
            .map(|parent| label(parent.name(), &fields_of(parent)))
            .collect();
        let record = json!({
            "kind": "span",
            "name": span.name(),
            "target": span.metadata().target(),
            "fields": info.fields,
            "spans": parents,
            "elapsed_ms": elapsed_ms(tap.started),
            "duration_ms": info.opened.elapsed().as_secs_f64() * 1000.0,
        });
        send(&tap, record);
    }
}

/// The fields this layer stored for `span`.
fn fields_of<S>(span: &tracing_subscriber::registry::SpanRef<'_, S>) -> Map<String, Value>
where
    S: for<'a> LookupSpan<'a>,
{
    span.extensions()
        .get::<SpanInfo>()
        .map(|info| info.fields.clone())
        .unwrap_or_default()
}

/// Milliseconds since `started`, with sub-millisecond precision.
fn elapsed_ms(started: Instant) -> f64 {
    started.elapsed().as_secs_f64() * 1000.0
}

/// Queues `record`, counting it when the client is too slow.
fn send(tap: &Tap, record: Value) {
    if tap.sender.try_send(record).is_err() {
        tap.dropped.fetch_add(1, Ordering::Relaxed);
    }
}

/// `name{key=value ...}`, the way spans are named in diagnostics.
fn label(name: &str, fields: &Map<String, Value>) -> String {
    if fields.is_empty() {
        return name.to_owned();
    }
    let mut text = format!("{name}{{");
    for (index, (key, value)) in fields.iter().enumerate() {
        if index > 0 {
            text.push(' ');
        }
        let value = value
            .as_str()
            .map_or_else(|| value.to_string(), str::to_owned);
        let _ = write!(text, "{key}={value}");
    }
    text.push('}');
    text
}

/// Collects a record's or span's fields; `message` and `tap` apart.
#[derive(Default)]
struct Fields {
    message: String,
    tap: Option<u64>,
    fields: Map<String, Value>,
}

impl Fields {
    /// Stores `value` under the field's name unless the name asks for redaction.
    fn put(&mut self, field: &Field, value: Value) {
        let name = field.name();
        let lower = name.to_ascii_lowercase();
        let value = if REDACTED.iter().any(|word| lower.contains(word)) {
            json!("[redacted]")
        } else {
            value
        };
        self.fields.insert(name.to_owned(), value);
    }
}

impl tracing::field::Visit for Fields {
    /// `tap` sets the tap id; other integers are kept as numbers.
    fn record_u64(&mut self, field: &Field, value: u64) {
        if field.name() == "tap" {
            self.tap = Some(value);
        } else {
            self.put(field, json!(value));
        }
    }

    /// Numbers stay numbers.
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.put(field, json!(value));
    }

    /// Numbers stay numbers.
    fn record_f64(&mut self, field: &Field, value: f64) {
        self.put(field, json!(value));
    }

    /// Booleans stay booleans.
    fn record_bool(&mut self, field: &Field, value: bool) {
        self.put(field, json!(value));
    }

    /// Strings as they are; the message apart.
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = value.to_owned();
        } else {
            self.put(field, json!(value));
        }
    }

    /// Everything else through `Debug` (`%value` arrives here too).
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.message = format!("{value:?}");
        } else {
            self.put(field, json!(format!("{value:?}")));
        }
    }
}

#[cfg(test)]
mod tests {
    use tracing_subscriber::layer::SubscriberExt;

    use super::*;

    #[test]
    /// Records below the tapped span arrive with their fields and spans; records
    /// outside it, above the tap's level or in redacted fields do not.
    fn taps_receive_their_request_only() {
        let subscriber = tracing_subscriber::registry().with(layer());
        tracing::subscriber::with_default(subscriber, || {
            let (guard, mut receiver) = register(1);
            const TARGET: &str = "devlanes_daemon::lifecycle";
            let span = tracing::info_span!(
                target: "devlanes_daemon::request",
                "request",
                tap = guard.id(),
                method = "start"
            );
            span.in_scope(|| {
                let phase = tracing::debug_span!(target: TARGET, "phase", name = "infra");
                phase.in_scope(|| {
                    tracing::debug!(target: TARGET, program = "docker", token = "s3cr3t", "running");
                    tracing::trace!(target: TARGET, "too detailed");
                });
            });
            tracing::debug!(target: "devlanes_daemon::lifecycle", "outside the request");
            drop(span);
            let dropped = guard.finish();
            assert_eq!(dropped, 0);
            let mut records = Vec::new();
            while let Ok(record) = receiver.try_recv() {
                records.push(record);
            }
            let text = serde_json::to_string(&records).expect("json");
            assert!(!text.contains("s3cr3t"), "{text}");
            assert!(!text.contains("too detailed"), "{text}");
            assert!(!text.contains("outside the request"), "{text}");
            let running = records
                .iter()
                .find(|r| r["message"] == "running")
                .expect("record");
            assert_eq!(running["level"], "debug");
            assert_eq!(running["fields"]["program"], "docker");
            assert_eq!(running["fields"]["token"], "[redacted]");
            assert_eq!(
                running["spans"],
                json!(["request{method=start}", "phase{name=infra}"])
            );
            let closed = records
                .iter()
                .find(|r| r["kind"] == "span" && r["name"] == "phase")
                .expect("span close");
            assert!(closed["duration_ms"].is_number());
        });
    }
}
