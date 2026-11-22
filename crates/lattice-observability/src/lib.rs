//! NAI Lattice canonical observability ledger (namespace `observability.log.v1`).
//!
//! Every NAI component — Rust backend, Node sidecars (Mastra/LUSI/future), and
//! WebView2 widget consoles (which already funnel through the Rust `log` crate)
//! — emits structured log records here. The fabric host appends them to the
//! canonical ledger as `observability.log.v1` events, and any runtime queries
//! them back for tail / search / by-trace. This is the Kibana-style line: each
//! record carries `ts_ms`, `level`, `component` (who logged — a Rust module, a
//! webview label, or a `sidecar[pid]` tag), `trace_id` (the request/correlation
//! being processed), `caller` (who called whom when a span context is attached),
//! and the free-form `message` + `extras`.
//!
//! == Why this exists ==
//! Before this, logs were ad-hoc: per-event append/reconnect, lock/delete churn,
//! idempotency bookkeeping, and the frontend's `console.*` went to a text log
//! nobody could search. This crate is the ONE ingestion contract: producers enqueue
//! into a bounded ring, a flusher batches them into `event.append_batch`, and the
//! ledger holds them append-only (never deleted). The AI agent queries the same
//! stream directly — no need to ask a human to open DevTools.

use serde_json::{json, Value};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Canonical Lattice namespace for structured logs.
pub const LOG_NAMESPACE: &str = "observability.log.v1";

/// Canonical log level vocabulary (matches the NAI OS frontend `LogLevel` order).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Trace = 1,
    Debug = 2,
    Info = 3,
    Warn = 4,
    Error = 5,
}

impl Level {
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => Level::Trace,
            2 => Level::Debug,
            3 => Level::Info,
            4 => Level::Warn,
            5 => Level::Error,
            _ => Level::Info,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Trace => "trace",
            Level::Debug => "debug",
            Level::Info => "info",
            Level::Warn => "warn",
            Level::Error => "error",
        }
    }
    pub fn at_least(self, min: Level) -> bool {
        self >= min
    }
}

static LOG_SEQUENCE: AtomicU64 = AtomicU64::new(1);

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// One canonical structured log record — the exact JSON stored as the
/// `observability.log.v1` event payload.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LogRecord {
    /// Process-local monotonic record identity, persisted for idempotent batch retries.
    #[serde(default)]
    pub producer_seq: u64,
    #[serde(default)]
    pub ts_ms: u64,
    #[serde(default)]
    pub level: String,
    /// Who logged: a Rust module path, a webview label, or `sidecar[pid]`.
    pub component: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    /// Span context of "who called whom" when a request scope is attached.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caller: Option<String>,
    pub message: String,
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub extras: serde_json::Map<String, Value>,
}

impl LogRecord {
    /// Build a record with monotonic-ish timestamp, level and component context.
    pub fn new(level: Level, component: &str, message: impl Into<String>) -> Self {
        Self {
            producer_seq: LOG_SEQUENCE.fetch_add(1, Ordering::Relaxed),
            ts_ms: now_ms(),
            level: level.as_str().to_string(),
            component: component.to_string(),
            trace_id: None,
            caller: None,
            message: message.into(),
            extras: serde_json::Map::new(),
        }
    }

    pub fn with_trace(mut self, trace_id: impl Into<String>) -> Self {
        self.trace_id = Some(trace_id.into());
        self
    }
    pub fn with_caller(mut self, caller: impl Into<String>) -> Self {
        self.caller = Some(caller.into());
        self
    }
    pub fn with_extra(mut self, key: &str, value: Value) -> Self {
        self.extras.insert(key.to_string(), value);
        self
    }
}

/// Build the `event.append_batch` args JSON for a set of records. `producer` is
/// the durable identity of the emitting component (e.g. `nai-os`, `npilot-agent`),
/// and each record gets `durability: "durable"` + a deterministic idempotency key
/// (component + trace + ts + a hash of the message) so a retry never duplicates.
pub fn build_batch_args(producer: &str, records: &[LogRecord]) -> Value {
    let events: Vec<Value> = records
        .iter()
        .map(|r| {
            server_payload(r, producer)
        })
        .collect();
    json!({
        "namespace": LOG_NAMESPACE,
        "event_type": "log.record",
        "entity_id": format!("observability/log/v1/{producer}"),
        "events": events,
    })
}

/// One item in the `event.append_batch` array.
fn server_payload(r: &LogRecord, producer: &str) -> Value {
    let idem = format!(
        "{}||{}||{}||{}||{}||{}",
        producer,
        std::process::id(),
        r.producer_seq,
        r.ts_ms,
        r.component,
        hash_message(&r.message)
    );
    json!({
        "namespace": "observability.log.v1",
        "event_type": "log.record",
        "entity_id": format!("observability/log/v1/{producer}"),
        "correlation_id": r.trace_id.clone().unwrap_or_default(),
        "payload": json!({
            "ts_ms": r.ts_ms,
            "producer_seq": r.producer_seq,
            "level": r.level,
            "component": r.component,
            "trace_id": r.trace_id,
            "caller": r.caller,
            "message": r.message,
            "extras": r.extras,
        }),
        "durability": "durable",
        "idempotency_key": idem,
    })
}

/// Short, deterministic message hash (for idempotency keys, not crypto).
fn hash_message(msg: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    msg.hash(&mut h);
    format!("{:016x}", h.finish())
}

/// Parse a `log.record` event payload JSON back into a `LogRecord`.
pub fn parse_record(payload: &Value) -> Option<LogRecord> {
    let mut rec: LogRecord = serde_json::from_value(payload.clone()).ok()?;
    if rec.ts_ms == 0 {
        rec.ts_ms = now_ms();
    }
    Some(rec)
}

// ===========================================================================
// LogEmitter — bounded ring + background batched flusher (non-blocking emit)
// ===========================================================================

/// A sink abstraction over a connected NAF client dispatch call, so the emitter
/// can be unit-tested with an in-memory sink and the real thing talks to the
/// fabric host without depending on a concrete client in the hot path.
pub trait LogSink: Send + Sync + 'static {
    /// Persist a batch of records (blocking call from the flusher thread).
    fn append_batch(&self, producer: &str, records: &[LogRecord]) -> Result<(), String>;
}

/// A reactor-bound NAF client. Tokio Windows named pipes register their IOCP
/// handle with the reactor where connect ran: dispatch MUST run on that same
/// runtime, not on a second Tokio runtime created in a sink constructor.
pub type DynNafClient = nai_lattice_client::NafClient<Box<dyn nai_lattice_client::transport::NafTransportTrait>>;
type ConnectFuture = std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<DynNafClient, String>> + Send>,
>;
type Reconnector = dyn Fn() -> ConnectFuture + Send + Sync;

/// Canonical durable batch sink. The only blocking caller is LogEmitter's
/// dedicated worker; producers never enter this sink. Failed or timed-out
/// dispatches discard the protocol stream so a later retry must handshake
/// cleanly instead of reading a response to an abandoned request.
pub struct NafSink {
    client: Mutex<Option<DynNafClient>>,
    reactor: tokio::runtime::Handle,
    reconnect: Option<Box<Reconnector>>,
    threshold: Level,
    _producer: String,
}

impl NafSink {
    /// Must be constructed inside the SAME Tokio runtime that connected the
    /// client. This is intentional and avoids moving IOCP handles across
    /// unrelated reactors.
    pub fn new(client: DynNafClient, producer: &str) -> Self {
        let reactor = tokio::runtime::Handle::try_current()
            .expect("NafSink::new must run on the NAF client's Tokio reactor");
        Self {
            client: Mutex::new(Some(client)),
            reactor,
            reconnect: None,
            threshold: Level::Trace,
            _producer: producer.to_string(),
        }
    }

    pub fn with_threshold(mut self, lvl: Level) -> Self {
        self.threshold = lvl;
        self
    }

    /// Provide a reconnect operation scheduled on the SAME reactor. Used by
    /// NAI OS so a host restart is recoverable without restarting the shell.
    pub fn with_reconnector<F, Fut>(mut self, connect: F) -> Self
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = Result<DynNafClient, String>> + Send + 'static,
    {
        self.reconnect = Some(Box::new(move || Box::pin(connect())));
        self
    }
}

impl LogSink for NafSink {
    fn append_batch(&self, producer: &str, records: &[LogRecord]) -> Result<(), String> {
        let keep: Vec<LogRecord> = records
            .iter()
            .filter(|r| Level::from_u8(level_u8(r)) >= self.threshold)
            .cloned()
            .collect();
        if keep.is_empty() {
            return Ok(());
        }
        let args = build_batch_args(producer, &keep);
        let body = serde_json::to_vec(&args).map_err(|e| e.to_string())?;
        let mut guard = self.client.lock().map_err(|e| e.to_string())?;
        if guard.is_none() {
            let reconnect = self.reconnect.as_ref()
                .ok_or_else(|| "NAF connection closed and no reconnector registered".to_string())?;
            let connected = self.reactor.block_on(async {
                tokio::time::timeout(Duration::from_secs(5), reconnect()).await
            }).map_err(|_| "NAF reconnect timed out".to_string())??;
            *guard = Some(connected);
        }
        let result = self.reactor.block_on(async {
            tokio::time::timeout(
                Duration::from_secs(5),
                guard.as_mut().expect("client connected").dispatch("event.append_batch", &body, None),
            ).await
        });
        match result {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(e)) => {
                *guard = None;
                Err(format!("NAF batch dispatch failed, connection reset: {e}"))
            }
            Err(_) => {
                *guard = None;
                Err("NAF batch timed out, connection reset".to_string())
            }
        }
    }
}

fn level_u8(r: &LogRecord) -> u8 {
    match r.level.as_str() {
        "trace" => 1,
        "debug" => 2,
        "info" => 3,
        "warn" => 4,
        "error" => 5,
        _ => 3,
    }
}

/// Batched producer: producers call `emit()` (non-blocking); a background thread
/// flushes every `flush_interval` (or when the ring hits `batch_size`) through the
/// `LogSink`. Bounded ring drops oldest under pressure (latest-state semantics),
/// so a burst never grows memory without limit (same policy as the NAI event bus).
pub struct LogEmitter {
    producer: String,
    sink: Box<dyn LogSink>,
    ring: Mutex<VecDeque<LogRecord>>,
    max_ring: usize,
    batch_size: usize,
    flush_interval: Duration,
    dropped: AtomicU64,
    flushed: AtomicU64,
    flush_requested: AtomicBool,
}

impl LogEmitter {
    pub fn new(producer: &str, sink: Box<dyn LogSink>) -> Self {
        let max_ring = 8192;
        let batch_size = 512;
        let flush_interval = Duration::from_millis(1000);
        Self {
            producer: producer.to_string(),
            sink,
            ring: Mutex::new(VecDeque::new()),
            max_ring,
            batch_size,
            flush_interval,
            dropped: AtomicU64::new(0),
            flushed: AtomicU64::new(0),
            flush_requested: AtomicBool::new(false),
        }
    }

    /// Fire-and-forget emit (non-blocking). Drops oldest if the ring is full.
    pub fn emit(&self, record: LogRecord) {
        let mut ring = self.ring.lock().unwrap();
        if ring.len() >= self.max_ring {
            ring.pop_front();
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
        ring.push_back(record);
    }

    /// Drain and flush whatever is queued (returns how many were sent).
    pub fn flush_now(&self) -> usize {
        let batch: Vec<LogRecord> = {
            let mut ring = self.ring.lock().unwrap();
            let n = ring.len().min(self.batch_size);
            let batch: Vec<LogRecord> = ring.drain(..n).collect();
            drop(ring);
            batch
        };
        if batch.is_empty() {
            return 0;
        }
        match self.sink.append_batch(&self.producer, &batch) {
            Ok(()) => {
                self.flushed.fetch_add(batch.len() as u64, Ordering::Relaxed);
                batch.len()
            }
            Err(e) => {
                // Sink down / send failed: re-queue the batch at the front so no
                // log is lost to a transient host drop (still bounded by the ring).
                tracing::warn!(
                    "lattice-observability: flush failed ({} records re-queued): {e}",
                    batch.len()
                );
                let mut ring = self.ring.lock().unwrap();
                for r in batch.into_iter().rev() {
                    if ring.len() >= self.max_ring {
                        ring.pop_back();
                        self.dropped.fetch_add(1, Ordering::Relaxed);
                    }
                    ring.push_front(r);
                }
                0
            }
        }
    }

    /// Ask the background flusher to drain soon. Safe from UI/logger threads;
    /// unlike flush_now this never calls NAF synchronously.
    pub fn request_flush(&self) {
        self.flush_requested.store(true, Ordering::Release);
    }

    /// Bounded worker: batch on cadence/pressure, exponential retry on a dead
    /// fabric, and never dispatch from a log producer or UI thread.
    pub fn run_forever(&self, stop: std::sync::Arc<AtomicBool>) {
        let mut last_flush = std::time::Instant::now();
        let mut next_retry = std::time::Instant::now();
        let mut retry = Duration::from_secs(1);
        while !stop.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(100));
            let queued = self.ring.lock().unwrap_or_else(|e| e.into_inner()).len();
            if queued == 0 {
                last_flush = std::time::Instant::now();
                continue;
            }
            let forced = self.flush_requested.swap(false, Ordering::AcqRel);
            let now = std::time::Instant::now();
            if now < next_retry {
                // Honor recovery backoff even if producers are flooding us.
                continue;
            }
            if !forced && queued < self.batch_size && last_flush.elapsed() < self.flush_interval {
                continue;
            }
            let sent = self.flush_now();
            last_flush = now;
            if sent == 0 {
                next_retry = now + retry;
                retry = (retry * 2).min(Duration::from_secs(30));
            } else {
                next_retry = now;
                retry = Duration::from_secs(1);
            }
        }
        // On orderly shutdown make one final best-effort attempt.
        self.flush_now();
    }

    pub fn stats(&self) -> Value {
        json!({
            "producer": self.producer,
            "queued": self.ring.lock().unwrap().len(),
            "dropped": self.dropped.load(Ordering::Relaxed),
            "flushed": self.flushed.load(Ordering::Relaxed),
            "flushRequested": self.flush_requested.load(Ordering::Relaxed),
        })
    }
}

// ===========================================================================
// Query helpers — read `observability.log.v1` events back from a runtime
// ===========================================================================
//
// These operate on the raw array of log records as returned by
// `event.read_range` (namespace observability.log.v1). A caller fetches with the
// runtime/client and then filters here; the filters are pure and testable.

/// Filter `LogRecord`s by criteria (tail-friendly).
#[allow(clippy::too_many_arguments)]
pub fn filter<'a>(
    records: &'a [LogRecord],
    level_min: Option<Level>,
    component_eq: Option<&str>,
    contains: Option<&str>,
    trace_id_eq: Option<&str>,
    from_ms: Option<u64>,
    to_ms: Option<u64>,
    limit: usize,
) -> Vec<&'a LogRecord> {
    let mut out: Vec<&LogRecord> = records
        .iter()
        .filter(|r| {
            if let Some(l) = level_min {
                if Level::from_u8(level_u8(r)) < l {
                    return false;
                }
            }
            if let Some(c) = component_eq {
                if r.component != c {
                    return false;
                }
            }
            if let Some(c) = contains {
                if !r.message.to_lowercase().contains(&c.to_lowercase())
                    && !r.level.contains(c)
                {
                    return false;
                }
            }
            if let Some(t) = trace_id_eq {
                if r.trace_id.as_deref() != Some(t) {
                    return false;
                }
            }
            if let Some(f) = from_ms {
                if r.ts_ms < f {
                    return false;
                }
            }
            if let Some(t) = to_ms {
                if r.ts_ms > t {
                    return false;
                }
            }
            true
        })
        .collect();
    // Newest-first (tail semantics).
    out.sort_by(|a, b| b.ts_ms.cmp(&a.ts_ms));
    out.truncate(limit);
    out
}
#[cfg(test)]
mod regression_tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn repeated_same_millisecond_identical_logs_are_distinct() {
        let mut a = LogRecord::new(Level::Warn, "npilot", "gateway unavailable");
        let mut b = LogRecord::new(Level::Warn, "npilot", "gateway unavailable");
        a.ts_ms = 1234567;
        b.ts_ms = 1234567;
        let first = build_batch_args("nai-os", &[a, b]);
        let keys: Vec<&str> = first["events"].as_array().unwrap().iter()
            .map(|e| e["idempotency_key"].as_str().unwrap()).collect();
        assert_eq!(keys.len(), 2);
        assert_ne!(keys[0], keys[1]);
    }

    struct ProbeSink {
        calls: Arc<AtomicUsize>,
        successes: Arc<Mutex<Vec<LogRecord>>>,
        fail_first: bool,
    }
    impl LogSink for ProbeSink {
        fn append_batch(&self, _producer: &str, records: &[LogRecord]) -> Result<(), String> {
            let call = self.calls.fetch_add(1, Ordering::Relaxed);
            if self.fail_first && call == 0 { return Err("host down".into()) }
            self.successes.lock().unwrap().extend_from_slice(records);
            Ok(())
        }
    }

    #[test]
    fn queued_events_survive_transient_sink_failure() {
        let calls = Arc::new(AtomicUsize::new(0));
        let saved = Arc::new(Mutex::new(Vec::new()));
        let emitter = LogEmitter::new("test", Box::new(ProbeSink {
            calls: calls.clone(), successes: saved.clone(), fail_first: true,
        }));
        emitter.emit(LogRecord::new(Level::Info, "npilot", "one"));
        emitter.emit(LogRecord::new(Level::Info, "npilot", "two"));
        assert_eq!(emitter.flush_now(), 0);
        assert_eq!(emitter.stats()["queued"], 2);
        assert_eq!(emitter.flush_now(), 2);
        assert_eq!(saved.lock().unwrap().len(), 2);
        assert_eq!(emitter.stats()["flushed"], 2);
        assert_eq!(calls.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn hot_path_flush_request_never_calls_sink() {
        let calls = Arc::new(AtomicUsize::new(0));
        let emitter = LogEmitter::new("test", Box::new(ProbeSink {
            calls: calls.clone(), successes: Arc::new(Mutex::new(Vec::new())), fail_first: false,
        }));
        emitter.emit(LogRecord::new(Level::Debug, "dock", "frame"));
        emitter.request_flush();
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        assert_eq!(emitter.stats()["flushRequested"], true);
    }
}
