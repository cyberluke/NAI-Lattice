# Contributing to NAI-Lattice

Thank you for considering a contribution. NAI-Lattice is the event-sourcing,
CQRS multi-dimensional datalake at the heart of the NAI Ambient Fabric. It is
open source so the community can run enterprise-grade local event storage and
query without a cloud dependency.

## Ground rules

- **SurrealKV stays the authoritative event store.** We do not migrate away from
  it. Query accelerators, projections, and indexes are read-side CQRS
  projections layered over the canonical ledger — never a second write path.
- **Write path purity.** The command side (`Ledger::append*`) owns NO read
  index, NO log tail, NO dashboard cache. Any read accelerator belongs in a read
  model (`read_model.rs`, `multi_dim.rs`) with its own lock/bulkhead. Do not
  couple the high-frequency read path to the append mutex.
- **Idempotent replay, always.** Every projection must restart from a durable
  cursor (`applied_high_water`) and re-ingest only the true delta — never
  duplicate, never cold-scan the whole ledger on boot unless there is no cursor.
- **Bulletproof append.** Concurrency safety for appends is contract
  `LEDGER-001` (one mutex, linearizable) — do not add a second writer or a
  bypass.
- **Windows is first-class.** The product targets modern Windows 11
  workstations. Keep builds green on `cargo build --release` (we ship a release
  MSIX / binary only) and run `cargo test -p nai-lattice-ledger -p nai-lattice-runtime`.

## Development flow

1. **Fork + branch.** Open a PR against `main`. Keep the change scoped to one
   concern (one projection, one seam, one fix).
2. **Commit style.** Lowercase, imperative, focused: `add multi-dim index
   projection`, not `updates`. Reference the contract you touch
   (`LEDGER-00x`, `NAF-00x`) when relevant.
3. **Test.** Every new read model / projection ships with unit tests for:
   - fresh index & query correctness,
   - idempotent replay (re-sync must not duplicate),
   - durable-cursor restore across reopen,
   - and (for any index) a small benchmark against a full scan so we know the
     projection is worth its memory.
4. **Version.** Ship with a version bump in `Cargo.toml` of the host + any
   workspace lock — a delivery at the same version is treated as no-op by the
   runtime.
5. **Provenance.** If you add a dependency, verify license, maintenance, Windows
   support, and size. We prefer small, battle-tested Crates over large engines.

## Contracts you may touch

- `LEDGER-001` – single append lock, linearizable ordering.
- `LEDGER-002` – BatchedDurable via the ledger's own persistent writer.
- `LEDGER-003` – no external flush hook.
- `CQRS-*` – read models own their bulkheads; projections replay idempotently.
- `NAF-00x` – protocol surface (`event.read_range`, `index.query`, ...).

## Code of conduct

Be civil. This is a small, high-trust project; we assume good intent and expect
disagreements to stay technical. Harassment of any kind is not tolerated.

## License

Contributions are accepted under the repository's [Apache-2.0](LICENSE) license.
By submitting a PR you agree that your contribution is licensed under it.