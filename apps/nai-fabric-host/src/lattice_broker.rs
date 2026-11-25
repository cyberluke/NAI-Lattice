//! Shared SSE scan broker for `/lattice/events`.
//!
//! One background pump thread ticks every ~1250 ms and performs ONE bounded
//! `logs_after(cursor)` read for the whole subscriber set — never one ledger
//! scanner per connection. Each subscriber owns a bounded `sync_channel(256)`;
//! a full queue counts a drop and is skipped (the pump never blocks). The
//! subscriber's cursor advances past the skipped batch so one slow consumer
//! cannot starve the others; its next reconnect resumes from the last actually
//! delivered `id:` (contract §4).
//!
//! Messages:
//!  - `search`  initial bounded snapshot (max 4096) at subscribe;
//!  - `log`     deltas, `id:` = last delivered `runtime_sequence`;
//!  - `stats`   one computation every ~4 ticks, fanned out to every subscriber;
//!  - `state`   runtime health `state` transitions (pass-through only);
//!  - `status`  per-connection truth every tick;
//!  - heartbeat comments on quiet ticks.
//!
//! The pump is testable synchronously via [`Broker::pump_once`] — tests never
//! depend on the wall-clock thread.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use serde_json::{json, Value};

/// Hard cap on simultaneous `/lattice/events` subscribers. Over the cap the
/// route answers `503` + `Retry-After: 3` — never a silently dead stream.
pub(crate) const MAX_SUBSCRIBERS: usize = 24;
/// Per-subscriber queue depth (messages, not records).
const SUBSCRIBER_QUEUE: usize = 256;
/// Pump cadence.
const TICK: Duration = Duration::from_millis(1_250);
/// Per-tick ledger read bound (records, both namespaces merged).
const DELTA_MAX: usize = 4_096;
/// Initial snapshot bound.
const SNAPSHOT_MAX: usize = 4_096;
/// Cursor-less first connect gets the newest bounded tail (legacy behaviour).
const INITIAL_TAIL: usize = 200;
/// `stats` fan-out cadence (ticks).
const STATS_EVERY: u64 = 4;
pub(crate) const RETRY_AFTER_SECS: u64 = 3;
/// Hard connection lifetime; the client resumes with `Last-Event-ID`.
const STREAM_LIFETIME: Duration = Duration::from_secs(600);

/// One message pushed to a subscriber channel.
#[derive(Clone)]
pub(crate) enum BrokerMsg {
    /// Initial snapshot: JSON array of records.
    Search {
        id: Option<u64>,
        payload: Arc<str>,
    },
    /// Delta batch: `{"hits":[records]}`.
    Log { id: u64, payload: Arc<str> },
    /// Pre-computed stats payload shared by every subscriber.
    Stats(Arc<str>),
    /// Runtime health state transition, pass-through payload.
    State(Arc<str>),
    /// Per-connection truth.
    Status {
        cursor: u64,
        subscribers: usize,
        dropped_total: u64,
        broker_scans: u64,
    },
    /// SSE comment; keeps dead peers detectable on quiet ticks.
    Heartbeat,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BrokerStats {
    pub subscribers: usize,
    pub scans: u64,
    pub dropped_total: u64,
    pub last_scan_unix_ms: u64,
}

/// Ledger source seam so the pump can be unit-tested with a synthetic source
/// and no runtime/network.
pub(crate) trait LogSource: Send + Sync + 'static {
    /// Up to `limit` records after an exclusive cursor, globally ascending.
    fn logs_after(&self, after: u64, limit: usize) -> Vec<Value>;
    /// Newest bounded window, ascending.
    fn latest(&self, limit: usize) -> Vec<Value>;
    /// Runtime health JSON (only `state`/`detail`/`sinceUnixMs` are consumed).
    fn health(&self) -> Value;
}

/// Production source: bounded reads over the process runtime.
pub(crate) struct RuntimeLogSource;

impl LogSource for RuntimeLogSource {
    fn logs_after(&self, after: u64, limit: usize) -> Vec<Value> {
        match crate::gastown_orto::RUNTIME.get() {
            Some(rt) => crate::lattice_query::logs_after_on(rt, after, limit),
            None => Vec::new(),
        }
    }

    fn latest(&self, limit: usize) -> Vec<Value> {
        match crate::gastown_orto::RUNTIME.get() {
            Some(rt) => crate::lattice_query::latest_logs_on(rt, limit),
            None => Vec::new(),
        }
    }

    fn health(&self) -> Value {
        match crate::gastown_orto::RUNTIME.get() {
            Some(rt) => rt.health(),
            None => Value::Null,
        }
    }
}

struct SubEntry {
    tx: SyncSender<BrokerMsg>,
    cursor: u64,
}

struct Inner {
    subs: BTreeMap<u64, SubEntry>,
    next_id: u64,
    tick: u64,
    last_state: Option<String>,
}

pub(crate) struct Broker {
    source: Arc<dyn LogSource>,
    inner: Mutex<Inner>,
    scans: AtomicU64,
    dropped: AtomicU64,
    last_scan_unix_ms: AtomicI64,
    queue: usize,
    auto_pump: bool,
    pump_started: AtomicBool,
    subscriber_gauge: AtomicUsize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SubscribeError {
    /// Global subscriber cap reached (`503`, `Retry-After: 3`).
    AtCapacity,
}

/// Held by the SSE connection; dropping it unregisters the subscriber.
pub(crate) struct Subscription {
    id: u64,
    rx: Receiver<BrokerMsg>,
    broker: Arc<Broker>,
}

impl Subscription {
    /// Bounded wait for the next message (the connection loop owns cadence).
    pub(crate) fn recv_timeout(
        &self,
        timeout: Duration,
    ) -> Result<BrokerMsg, RecvTimeoutError> {
        self.rx.recv_timeout(timeout)
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        let removed = self
            .broker
            .inner
            .lock()
            .map(|mut g| g.subs.remove(&self.id).is_some())
            .unwrap_or(false);
        if removed {
            self.broker
                .subscriber_gauge
                .fetch_sub(1, Ordering::AcqRel);
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PumpReport {
    pub subscribers: usize,
    pub scanned_records: usize,
    pub delivered_records: usize,
    pub dropped_records: usize,
}

impl Broker {
    pub(crate) fn new(source: Arc<dyn LogSource>) -> Arc<Self> {
        Self::build(source, SUBSCRIBER_QUEUE, true)
    }

    /// Manual-pump broker for deterministic tests (no background thread).
    #[cfg(test)]
    fn manual(source: Arc<dyn LogSource>, queue: usize) -> Arc<Self> {
        Self::build(Arc::clone(&source), queue, false)
    }

    fn build(source: Arc<dyn LogSource>, queue: usize, auto_pump: bool) -> Arc<Self> {
        Arc::new(Self {
            source,
            inner: Mutex::new(Inner {
                subs: BTreeMap::new(),
                next_id: 1,
                tick: 0,
                last_state: None,
            }),
            scans: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            last_scan_unix_ms: AtomicI64::new(0),
            queue: queue.max(1),
            auto_pump,
            pump_started: AtomicBool::new(false),
            subscriber_gauge: AtomicUsize::new(0),
        })
    }

    pub(crate) fn stats(&self) -> BrokerStats {
        let subscribers = self.subscriber_gauge.load(Ordering::Acquire);
        BrokerStats {
            subscribers,
            scans: self.scans.load(Ordering::Relaxed),
            dropped_total: self.dropped.load(Ordering::Relaxed),
            last_scan_unix_ms: self.last_scan_unix_ms.load(Ordering::Relaxed).max(0) as u64,
        }
    }

    fn at_capacity(&self) -> bool {
        self.subscriber_gauge.load(Ordering::Acquire) >= MAX_SUBSCRIBERS
    }

    /// Register a subscriber. The initial snapshot is read BEFORE registration;
    /// the pump always reads from the minimum registered cursor, so records
    /// appended between the snapshot and registration are delivered on the next
    /// tick — never lost and never reordered before the snapshot.
    pub(crate) fn subscribe(
        self: &Arc<Self>,
        after: Option<u64>,
    ) -> Result<Subscription, SubscribeError> {
        if self.at_capacity() {
            return Err(SubscribeError::AtCapacity);
        }
        let snapshot = match after {
            Some(a) => self.source.logs_after(a, SNAPSHOT_MAX),
            None => self.source.latest(INITIAL_TAIL),
        };
        let mut snapshot = snapshot;
        snapshot.sort_by_key(crate::lattice_query::seq_of);
        snapshot.dedup_by_key(|r| crate::lattice_query::seq_of(r));
        if snapshot.len() > SNAPSHOT_MAX {
            snapshot.truncate(SNAPSHOT_MAX);
        }
        let snapshot_id = snapshot.iter().map(crate::lattice_query::seq_of).max();
        let cursor = snapshot_id.unwrap_or(0).max(after.unwrap_or(0));
        let payload: Arc<str> =
            Arc::from(serde_json::to_string(&snapshot).unwrap_or_else(|_| "[]".to_string()));

        let (tx, rx) = sync_channel(self.queue);
        let id = {
            let mut g = match self.inner.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            if g.subs.len() >= MAX_SUBSCRIBERS {
                return Err(SubscribeError::AtCapacity);
            }
            let id = g.next_id;
            g.next_id += 1;
            g.subs.insert(
                id,
                SubEntry {
                    tx: tx.clone(),
                    cursor,
                },
            );
            id
        };
        self.subscriber_gauge.fetch_add(1, Ordering::AcqRel);
        // The snapshot is the only per-subscriber read; the channel was just
        // created, so it cannot be full.
        let _ = tx.try_send(BrokerMsg::Search {
            id: snapshot_id,
            payload,
        });
        self.ensure_pump();
        Ok(Subscription {
            id,
            rx,
            broker: Arc::clone(self),
        })
    }

    fn ensure_pump(self: &Arc<Self>) {
        if !self.auto_pump {
            return;
        }
        if self.pump_started.swap(true, Ordering::SeqCst) {
            return;
        }
        let broker = Arc::clone(self);
        let spawned = std::thread::Builder::new()
            .name("lattice-sse-broker".into())
            .spawn(move || loop {
                std::thread::sleep(TICK);
                broker.pump_once();
            });
        if let Err(e) = spawned {
            self.pump_started.store(false, Ordering::SeqCst);
            tracing::warn!(error = %e, "could not start the shared Lattice SSE broker thread");
        }
    }

    /// One pump tick. Reads the ledger ONCE for every subscriber, fans out the
    /// delta, one shared `stats` computation every ~4 ticks, one `state`
    /// transition, one per-connection `status`, and a heartbeat on quiet ticks.
    /// The registry lock is held only to snapshot subscribers and to commit
    /// cursor advancement/removals. Filtering, cloning and JSON serialization
    /// happen outside it, so a slow subscriber payload never blocks
    /// `subscribe`/`Drop` or the stats path. An idle broker returns before any
    /// runtime dispatch or file I/O.
    pub(crate) fn pump_once(&self) -> PumpReport {
        // Idle brokers must be cheap: check the subscriber set BEFORE
        // `source.health()` and the ledger read, and do not advance the tick.
        let (snapshot, tick) = {
            let mut g = match self.inner.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            if g.subs.is_empty() {
                return PumpReport::default();
            }
            g.tick = g.tick.wrapping_add(1);
            let tick = g.tick;
            let snapshot: Vec<(u64, u64, SyncSender<BrokerMsg>)> = g
                .subs
                .iter()
                .map(|(id, sub)| (*id, sub.cursor, sub.tx.clone()))
                .collect();
            (snapshot, tick)
        };
        let subscribers = snapshot.len();
        let min_cursor = snapshot
            .iter()
            .map(|(_, cursor, _)| *cursor)
            .min()
            .unwrap_or(0);

        let health = self.source.health();

        let mut batch = self.source.logs_after(min_cursor, DELTA_MAX);
        batch.sort_by_key(crate::lattice_query::seq_of);
        batch.dedup_by_key(|r| crate::lattice_query::seq_of(r));
        batch.retain(|r| crate::lattice_query::seq_of(r) > min_cursor);

        self.scans.fetch_add(1, Ordering::Relaxed);
        self.last_scan_unix_ms
            .store(crate::now_unix_ms(), Ordering::Relaxed);

        // ONE stats computation per cadence, shared by every subscriber. This is
        // deliberately outside the registry lock (stats() locks it briefly).
        let stats_payload: Option<Arc<str>> = (tick % STATS_EVERY == 0).then(|| {
            let s = crate::lattice_dashboard::stats();
            Arc::<str>::from(serde_json::to_string(&s).unwrap_or_else(|_| "{}".to_string()))
        });

        // State is broadcast only on a real transition of the runtime-reported
        // `state`; `detail`/`sinceUnixMs`/`etaMs` are passed through, never
        // synthesized.
        let state_payload: Option<Arc<str>> = health
            .get("state")
            .and_then(Value::as_str)
            .and_then(|state| {
                let changed = {
                    let mut g = match self.inner.lock() {
                        Ok(g) => g,
                        Err(p) => p.into_inner(),
                    };
                    if g.last_state.as_deref() == Some(state) {
                        false
                    } else {
                        g.last_state = Some(state.to_string());
                        true
                    }
                };
                changed.then(|| {
                    let mut obj = serde_json::Map::new();
                    obj.insert("state".into(), Value::String(state.to_string()));
                    obj.insert(
                        "detail".into(),
                        health.get("detail").cloned().unwrap_or(Value::Null),
                    );
                    obj.insert(
                        "sinceUnixMs".into(),
                        health
                            .get("sinceUnixMs")
                            .or_else(|| health.get("since_unix_ms"))
                            .cloned()
                            .unwrap_or(Value::Null),
                    );
                    if let Some(eta) = health.get("etaMs").or_else(|| health.get("eta_ms")) {
                        obj.insert("etaMs".into(), eta.clone());
                    }
                    Arc::<str>::from(
                        serde_json::to_string(&Value::Object(obj))
                            .unwrap_or_else(|_| "{}".to_string()),
                    )
                })
            });

        let broker_scans = self.scans.load(Ordering::Relaxed);
        let dropped_before = self.dropped.load(Ordering::Relaxed);
        let mut report = PumpReport {
            subscribers,
            scanned_records: batch.len(),
            ..Default::default()
        };
        let mut dropped_now = 0usize;
        let mut dead: Vec<u64> = Vec::new();
        let mut cursor_updates: Vec<(u64, u64)> = Vec::with_capacity(subscribers);

        // Fan out OUTSIDE the registry lock: filtering, cloning and serializing
        // per subscriber must not block subscribe/Drop or the stats path.
        for (id, cursor, tx) in &snapshot {
            let mut next_cursor = *cursor;
            if !batch.is_empty() {
                let mine: Vec<Value> = batch
                    .iter()
                    .filter(|r| crate::lattice_query::seq_of(r) > *cursor)
                    .cloned()
                    .collect();
                let pending = mine.len();
                if pending > 0 {
                    let last_seq = mine
                        .last()
                        .map(crate::lattice_query::seq_of)
                        .unwrap_or(*cursor);
                    let payload: Arc<str> = Arc::from(
                        serde_json::to_string(&json!({ "hits": mine }))
                            .unwrap_or_else(|_| "{\"hits\":[]}".to_string()),
                    );
                    match tx.try_send(BrokerMsg::Log {
                        id: last_seq,
                        payload,
                    }) {
                        Ok(()) => {
                            next_cursor = last_seq;
                            report.delivered_records += pending;
                        }
                        Err(TrySendError::Full(_)) => {
                            // Backpressure: never block the pump. The gap is
                            // reported and the client resumes from its last
                            // delivered cursor on reconnect. Advancing the
                            // cursor keeps one slow consumer from starving
                            // the others.
                            dropped_now += pending;
                            next_cursor = last_seq;
                        }
                        Err(TrySendError::Disconnected(_)) => {
                            dead.push(*id);
                            continue;
                        }
                    }
                }
            }
            if let Some(p) = &stats_payload {
                let _ = tx.try_send(BrokerMsg::Stats(Arc::clone(p)));
            }
            if let Some(p) = &state_payload {
                let _ = tx.try_send(BrokerMsg::State(Arc::clone(p)));
            }
            if batch.is_empty() {
                let _ = tx.try_send(BrokerMsg::Heartbeat);
            }
            let _ = tx.try_send(BrokerMsg::Status {
                cursor: next_cursor,
                subscribers,
                dropped_total: dropped_before + dropped_now as u64,
                broker_scans,
            });
            cursor_updates.push((*id, next_cursor));
        }

        // Re-lock ONCE to commit cursors and remove disconnected subscribers.
        // A subscriber may already have removed itself via `Subscription::drop`
        // while we were fanning out; only count removals that WE performed so
        // the gauge is never decremented twice.
        let removed = {
            let mut g = match self.inner.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            for (id, next_cursor) in &cursor_updates {
                if let Some(sub) = g.subs.get_mut(id) {
                    if *next_cursor > sub.cursor {
                        sub.cursor = *next_cursor;
                    }
                }
            }
            let mut removed = 0usize;
            for id in &dead {
                if g.subs.remove(id).is_some() {
                    removed += 1;
                }
            }
            report.subscribers = g.subs.len();
            removed
        };

        if removed > 0 {
            self.subscriber_gauge
                .fetch_sub(removed, Ordering::AcqRel);
        }
        if dropped_now > 0 {
            self.dropped.fetch_add(dropped_now as u64, Ordering::Relaxed);
        }
        report.dropped_records = dropped_now;
        report
    }
}

// ===========================================================================
// Process-global broker
// ===========================================================================

static BROKER: OnceLock<Arc<Broker>> = OnceLock::new();

/// The process-wide broker (one pump for every SSE connection).
pub(crate) fn broker() -> &'static Arc<Broker> {
    BROKER.get_or_init(|| Broker::new(Arc::new(RuntimeLogSource)))
}

/// Broker counters without creating the broker (legacy stats/summary path).
pub(crate) fn snapshot_stats() -> BrokerStats {
    BROKER.get().map(|b| b.stats()).unwrap_or_default()
}

/// Hard connection lifetime for one SSE stream.
pub(crate) fn stream_lifetime() -> Duration {
    STREAM_LIFETIME
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeInner {
        records: Vec<Value>,
    }

    struct FakeSource {
        inner: Mutex<FakeInner>,
        calls: AtomicUsize,
        health_calls: AtomicUsize,
        health: Mutex<Value>,
    }

    impl FakeSource {
        fn new(seqs: &[u64]) -> Arc<Self> {
            Arc::new(Self {
                inner: Mutex::new(FakeInner {
                    records: seqs.iter().map(|s| record(*s)).collect(),
                }),
                calls: AtomicUsize::new(0),
                health_calls: AtomicUsize::new(0),
                health: Mutex::new(json!({"state": "healthy"})),
            })
        }

        fn push(&self, seqs: &[u64]) {
            let mut g = self.inner.lock().unwrap();
            g.records.extend(seqs.iter().map(|s| record(*s)));
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }

        fn health_calls(&self) -> usize {
            self.health_calls.load(Ordering::SeqCst)
        }

        fn reset_calls(&self) {
            self.calls.store(0, Ordering::SeqCst);
            self.health_calls.store(0, Ordering::SeqCst);
        }
    }

    fn record(seq: u64) -> Value {
        json!({
            "runtime_sequence": seq,
            "timestamp_utc_ms": 1_764_000_000_000u64 + seq,
            "level": "info",
            "component": "fake",
            "message": format!("record {seq}"),
        })
    }

    impl LogSource for FakeSource {
        fn logs_after(&self, after: u64, limit: usize) -> Vec<Value> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let g = self.inner.lock().unwrap();
            g.records
                .iter()
                .filter(|r| crate::lattice_query::seq_of(r) > after)
                .take(limit)
                .cloned()
                .collect()
        }

        fn latest(&self, limit: usize) -> Vec<Value> {
            let g = self.inner.lock().unwrap();
            let start = g.records.len().saturating_sub(limit);
            g.records[start..].to_vec()
        }

        fn health(&self) -> Value {
            self.health_calls.fetch_add(1, Ordering::SeqCst);
            self.health.lock().unwrap().clone()
        }
    }

    fn drain(rx: &Subscription) -> Vec<BrokerMsg> {
        let mut out = Vec::new();
        while let Ok(m) = rx.recv_timeout(Duration::from_millis(1)) {
            out.push(m);
        }
        out
    }

    fn as_source(source: &Arc<FakeSource>) -> Arc<dyn LogSource> {
        source.clone()
    }

    fn log_seqs(msgs: &[BrokerMsg]) -> Vec<u64> {
        let mut out = Vec::new();
        for m in msgs {
            if let BrokerMsg::Log { payload, .. } = m {
                let v: Value = serde_json::from_str(payload).unwrap();
                if let Some(hits) = v.get("hits").and_then(Value::as_array) {
                    out.extend(hits.iter().map(crate::lattice_query::seq_of));
                }
            }
        }
        out
    }

    #[test]
    fn twelve_subscribers_share_exactly_one_scan_per_tick_and_stay_ordered() {
        let source = FakeSource::new(&[1, 2, 3]);
        let broker = Broker::manual(as_source(&source), SUBSCRIBER_QUEUE);

        let mut subs = Vec::new();
        for _ in 0..12 {
            subs.push(broker.subscribe(Some(0)).expect("subscribe"));
        }
        for sub in &subs {
            let msgs = drain(sub);
            let snap = msgs
                .iter()
                .find_map(|m| match m {
                    BrokerMsg::Search { payload, id } => {
                        Some((serde_json::from_str::<Value>(payload).unwrap(), *id))
                    }
                    _ => None,
                })
                .expect("initial snapshot");
            assert_eq!(snap.0.as_array().unwrap().len(), 3);
            assert_eq!(snap.1, Some(3));
        }
        source.reset_calls();

        source.push(&[4, 5]);
        let report = broker.pump_once();
        assert_eq!(source.calls(), 1, "one ledger read serves every subscriber");
        assert_eq!(report.scanned_records, 2);
        assert_eq!(report.delivered_records, 2 * 12);
        for sub in &subs {
            let msgs = drain(sub);
            assert_eq!(log_seqs(&msgs), vec![4, 5]);
            let status = msgs
                .iter()
                .find_map(|m| match m {
                    BrokerMsg::Status { cursor, broker_scans, .. } => Some((*cursor, *broker_scans)),
                    _ => None,
                })
                .expect("status every tick");
            assert_eq!(status.0, 5, "cursor reflects the delivered delta");
            assert_eq!(status.1, 1);
        }

        source.push(&[6]);
        broker.pump_once();
        assert_eq!(source.calls(), 2);
        for sub in &subs {
            let msgs = drain(sub);
            assert_eq!(log_seqs(&msgs), vec![6], "deltas in order, no duplicates");
        }

        // Quiet tick: no read results -> heartbeat + status, still one scan.
        let report = broker.pump_once();
        assert_eq!(source.calls(), 3);
        assert_eq!(report.scanned_records, 0);
        for sub in &subs {
            let msgs = drain(sub);
            assert!(log_seqs(&msgs).is_empty());
            assert!(msgs
                .iter()
                .any(|m| matches!(m, BrokerMsg::Heartbeat)));
        }
        assert_eq!(broker.stats().subscribers, 12);
        assert_eq!(broker.stats().scans, 3);
    }

    #[test]
    fn full_channel_drops_without_blocking_and_advances_the_cursor() {
        let source = FakeSource::new(&[1]);
        // Queue of one message: the initial snapshot occupies it.
        let broker = Broker::manual(as_source(&source), 1);
        let sub = broker.subscribe(Some(0)).expect("subscribe");
        source.push(&[2, 3, 4]);
        let report = broker.pump_once();
        assert_eq!(report.dropped_records, 3, "whole delta skipped, counted");
        let stats = broker.stats();
        assert_eq!(stats.dropped_total, 3);
        assert_eq!(stats.subscribers, 1);
        // The only queued message is the initial snapshot; the stream is alive
        // and resumable from its `id:`.
        let msgs = drain(&sub);
        assert_eq!(msgs.len(), 1);
        match &msgs[0] {
            BrokerMsg::Search { id, .. } => assert_eq!(*id, Some(1)),
            other => panic!("expected search snapshot, got {other:?}", other = kind_of(other)),
        }
    }

    fn kind_of(m: &BrokerMsg) -> &'static str {
        match m {
            BrokerMsg::Search { .. } => "search",
            BrokerMsg::Log { .. } => "log",
            BrokerMsg::Stats(_) => "stats",
            BrokerMsg::State(_) => "state",
            BrokerMsg::Status { .. } => "status",
            BrokerMsg::Heartbeat => "heartbeat",
        }
    }

    #[test]
    fn subscriber_cap_is_enforced() {
        let source = FakeSource::new(&[]);
        let broker = Broker::manual(as_source(&source), 4);
        let mut subs = Vec::new();
        for _ in 0..MAX_SUBSCRIBERS {
            subs.push(broker.subscribe(None).expect("within cap"));
        }
        assert_eq!(broker.stats().subscribers, MAX_SUBSCRIBERS);
        assert!(matches!(
            broker.subscribe(None),
            Err(SubscribeError::AtCapacity)
        ));
        // Releasing exactly one slot admits exactly one more subscriber.
        drop(subs.pop());
        assert_eq!(broker.stats().subscribers, MAX_SUBSCRIBERS - 1);
        let last = broker.subscribe(None).expect("slot released");
        assert_eq!(broker.stats().subscribers, MAX_SUBSCRIBERS);
        drop(last);
        assert_eq!(broker.stats().subscribers, MAX_SUBSCRIBERS - 1);
    }

    #[test]
    fn resume_cursor_starts_after_delivered_snapshot() {
        let source = FakeSource::new(&[1, 2, 3, 4, 5]);
        let broker = Broker::manual(as_source(&source), SUBSCRIBER_QUEUE);
        let sub = broker.subscribe(Some(3)).expect("resume");
        let msgs = drain(&sub);
        let (records, id) = msgs
            .iter()
            .find_map(|m| match m {
                BrokerMsg::Search { payload, id } => Some((
                    serde_json::from_str::<Value>(payload).unwrap(),
                    *id,
                )),
                _ => None,
            })
            .expect("snapshot");
        let seqs: Vec<u64> = records
            .as_array()
            .unwrap()
            .iter()
            .map(crate::lattice_query::seq_of)
            .collect();
        assert_eq!(seqs, vec![4, 5], "resume never resets to zero");
        assert_eq!(id, Some(5));
        source.reset_calls();
        source.push(&[6]);
        broker.pump_once();
        assert_eq!(source.calls(), 1);
        assert_eq!(log_seqs(&drain(&sub)), vec![6]);
    }

    #[test]
    fn state_event_is_emitted_once_per_real_transition() {
        let source = FakeSource::new(&[1]);
        let broker = Broker::manual(as_source(&source), SUBSCRIBER_QUEUE);
        let sub = broker.subscribe(Some(0)).expect("subscribe");
        let _ = drain(&sub);
        // First tick observes the initial state.
        broker.pump_once();
        let msgs = drain(&sub);
        let states: Vec<String> = msgs
            .iter()
            .filter_map(|m| match m {
                BrokerMsg::State(p) => Some(p.to_string()),
                _ => None,
            })
            .collect();
        assert_eq!(states.len(), 1);
        let v: Value = serde_json::from_str(&states[0]).unwrap();
        assert_eq!(v["state"], "healthy");
        assert_eq!(v["detail"], Value::Null);
        // No transition -> no new state event.
        broker.pump_once();
        let msgs = drain(&sub);
        assert!(!msgs.iter().any(|m| matches!(m, BrokerMsg::State(_))));
        // Real transition -> exactly one event.
        *source.health.lock().unwrap() = json!({"state": "draining", "detail": "storage pressure"});
        broker.pump_once();
        let msgs = drain(&sub);
        let states: Vec<Value> = msgs
            .iter()
            .filter_map(|m| match m {
                BrokerMsg::State(p) => serde_json::from_str(p).ok(),
                _ => None,
            })
            .collect();
        assert_eq!(states.len(), 1);
        assert_eq!(states[0]["state"], "draining");
        assert_eq!(states[0]["detail"], "storage pressure");
    }

    #[test]
    fn stats_is_computed_once_for_every_subscriber_on_cadence() {
        let source = FakeSource::new(&[1]);
        let broker = Broker::manual(as_source(&source), SUBSCRIBER_QUEUE);
        let subs: Vec<Subscription> = (0..4)
            .map(|_| broker.subscribe(Some(1)).expect("subscribe"))
            .collect();
        for s in &subs {
            let _ = drain(s);
        }
        for tick in 1..=STATS_EVERY {
            broker.pump_once();
            let stats_count = subs
                .iter()
                .map(|s| {
                    drain(s)
                        .iter()
                        .filter(|m| matches!(m, BrokerMsg::Stats(_)))
                        .count()
                })
                .collect::<Vec<_>>();
            let expect = if tick % STATS_EVERY == 0 { 1 } else { 0 };
            assert_eq!(stats_count, vec![expect, expect, expect, expect]);
        }
    }

    #[test]
    fn no_subscribers_means_no_ledger_scan_or_health_dispatch() {
        let source = FakeSource::new(&[1, 2, 3]);
        let broker = Broker::manual(as_source(&source), SUBSCRIBER_QUEUE);
        source.reset_calls();
        let report = broker.pump_once();
        assert_eq!(report, PumpReport::default());
        assert_eq!(source.calls(), 0, "an idle broker never touches the ledger");
        assert_eq!(
            source.health_calls(),
            0,
            "an idle broker must not dispatch runtime.health"
        );
        // Still true after a real (non-empty) tick, then every subscriber leaves.
        let sub = broker.subscribe(Some(3)).expect("subscribe");
        broker.pump_once();
        assert_eq!(source.health_calls(), 1);
        drop(sub);
        source.reset_calls();
        assert_eq!(broker.pump_once(), PumpReport::default());
        assert_eq!(source.health_calls(), 0);
        assert_eq!(source.calls(), 0);
    }

    #[test]
    fn fan_out_filters_each_subscriber_against_its_own_cursor() {
        let source = FakeSource::new(&[1, 2]);
        let broker = Broker::manual(as_source(&source), SUBSCRIBER_QUEUE);
        let older = broker.subscribe(Some(0)).expect("older");
        let _ = drain(&older);

        // A later subscriber snapshots the current tail, so its cursor starts
        // ahead of the older subscriber's.
        source.push(&[3, 4]);
        let newer = broker.subscribe(Some(2)).expect("newer");
        let _ = drain(&newer);

        source.push(&[5]);
        let report = broker.pump_once();
        assert_eq!(report.subscribers, 2);
        assert_eq!(report.scanned_records, 3, "one scan serves both cursors");
        assert_eq!(log_seqs(&drain(&older)), vec![3, 4, 5]);
        assert_eq!(log_seqs(&drain(&newer)), vec![5]);
        assert_eq!(broker.stats().subscribers, 2);
    }
}
