# NAI-Lattice — Architecture

NAI-Lattice is the local-first, event-sourcing, CQRS multi-dimensional datalake
that powers the NAI Ambient Fabric observability plane. It is built to feel like
running a serious enterprise event store (SurrealKV-backed, schema'd,
replayable) on your own workstation — no cloud, no telemetry exfiltration.

## Principles

1. **Event sourcing.** The canonical ledger is an append-only, immutable,
   hash-chained event stream. Every fact anyone cares about is an event; nothing
   is ever overwritten or deleted. The write side is single-authority and
   linearizable (`LEDGER-001`): one append lock, deterministic sequence
   assignment, and a broken-chain detector in crash recovery.
2. **CQRS, run honestly.** The command side owns *nothing* read-side: no log
   tail, no dashboard cache, no seq→offset index inside the append mutex. All
   query accelerators are **read models** — separate projections with their own
   locks (bulkheads) that replay the ledger and keep their own materialized
   state. A slow reader can never block an append.
3. **Crash-consistent, delta replay.** Every projection keeps a durable cursor
   (`applied_high_water`) persisted atomically (tmp+rename). On restart it
   resumes from the cursor — O(restart-delta), never a cold full scan — and
   every replay is idempotent (a retry never duplicates).
4. **The storage engine is authoritative; indexes are projections.** SurrealKV /
   the event store stays the source of truth. Multi-dimensional indexes are
   CQRS projections over it. We deliberately did not hand-roll a second storage
   engine or adopt XTDB/LanceDB: the CTO sign-off (2026-11) chose "SurrealKV
   authoritative, multi-dim as materialized projections, split-ready seams."

## Layered view

```text
 ┌────────────────────────────────────────────────────────────┐
 │ Endpoints: NAF pipe (IPC), control HTTP /lattice, CLI, MCP  │
 ├────────────────────────────────────────────────────────────┤
 │ Query path (read side)                                      │
 │   event.read_range   index.query   log tail/search/stats    │
 │   ┌───────────────┐  ┌────────────────┐  ┌───────────────┐  │
 │   │ LedgerReadModel│  │ MultiDimIndex  │  │ dashboard    │  │
 │   │ hot-queue      │  │ time/index     │  │ (SSE/API)    │  │
 │   │ own Mutex      │  │ own RwLock     │  │ read-only    │  │
 │   └───────────────┘  └────────────────┘  └───────────────┘  │
 │         ▲ replay from durable cursor (idempotent)          │
 ├────────────────────────────────────────────────────────────┤
 │ Command path (write side) — ONE authoritative writer        │
 │   Ledger::append / append_batch                             │
 │   ┌──────────────────────────────────────────────────┐      │
 │   │ ActiveWriter (single Mutex, hash-chained)        │      │
 │   │   sequence alloc + prev-hash + flush, all atomic │      │
 │   └──────────────────────────────────────────────────┘      │
 │   SurrealKV hot store (authoritative) + segment ledgger     │
 └────────────────────────────────────────────────────────────┘
```

## The canonical ledger

- **Segment format.** Append-only `.segment` files (sealed by size target,
   default 128 MiB). Each frame carries the previous frame's BLAKE3 root, so the
   chain is tamper-evident (`§9 Segment format`). Crash recovery seals a torn
   tail from the last valid frame and replays forward.
- **Durability.** Three policies: `ProjectionOnly` (hot store), `BatchedDurable`
   (group-commit flush via the ledger's own persistent writer, `LEDGER-002/003`),
   and `Durable` (force flush). No external flush hook.
- **Namespace.** Records carry a namespace; the canonical observability stream
   is `observability.log.v1`. Writer-side namespace inheritance keeps producers
   from silently degrading every item into the `default` bucket (a real incident
   we fixed and regression-tested).

## Read models

### `LedgerReadModel` — the hot-queue projection

Bounded newest-first window over the ledger for the live dashboard/tail. Owns a
separate `Mutex`, restores from `read-model.json` on open, and catches up
`O(delta)` every tick.

### `MultiDimIndex` — the datalake index

Materialized B-tree projection answering first-class queries by
**namespace / entity / event-type / producer / correlation / time-range** without
scanning the ledger. Keys are **time-ordered** (`ts -> seqs`) so the hot
"newest-N of a key" query is `O(log + N)`, not all-matches-then-sort — the
benchmark over 1M events shows a **~68×** win over a full linear scan for a
single-producer query. Durability: `multi-dim-index.json` checkpoint (durable
cursor + keys), idempotent replay, own `RwLock`.

Both projections are fed by a single background worker that reconciles them to
the ledger high-water every 250 ms, each under its own bulkhead.

## Why not hand-rolled storage / XTDB / LanceDB

The first measurement is honest: a naive seq-set index was *slower* than a
linear scan for the trivially-cheap in-memory tail (because it collected every
match and sorted). We benchmarked, refactored to a time-keyed B-tree, and only
ship the projection once it beats the full-source scan on the query it exists to
serve. That discipline — measure, then project — is why the index has a distinct
value: **small result sets over very large ledgers, answered from a
materialized key instead of a pass over the whole event store.**

## Crash recovery contract

- Ledger: seal active segment tail → rebuild hash chain.
- Read models: restore durable cursor → replay missing delta (idempotently).
- No projection trusts `last_sequence()` alone; each holds its own high-water.

## Build & test

- `cargo build --release -p nai-fabric-host` — ship profile (we deliver the
  release binary/MSIX only).
- `cargo test -p nai-lattice-ledger -p nai-lattice-runtime` — command + projection
  suites, including the index benchmark.
- Contracts referenced above (`LEDGER-*`, `CQRS-*`, `NAF-*`) are inline doc
  anchors in the source.