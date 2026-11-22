# AGENTS.md — NAI-Lattice

## If you are an agent in a NAI product repo

You are shipping the shared fabric runtime, not a product-owned service.

1. **Read `docs/PRODUCT_BUNDLING.md` before writing any bundling or lifecycle
   code.** It is the canonical contract for every NAI product.
2. **Stage the host with**
   `packaging/bundle-host-for-product.ps1 -OutDir <product>\resources\lattice\host`
   **before** your product's resource hashing/packaging step (for example before
   `build.rs` writes `SHA256SUMS` or the MSIX bundler copies `static/`).
3. **Never use the optional `helldesk-release` developer tooling** in a product
   lifecycle, and never write `releases\current.json`. That layout is optional
   and not part of the shared contract.
4. **Shared contract (do not fork it):**
   - `%LOCALAPPDATA%\NAI\Lattice\active.json` — shared version pointer, written
     atomically and last, under the manager mutex.
   - `%LOCALAPPDATA%\NAI\Lattice\runtime\<version>\nai-fabric-host.exe` —
     installed artifacts.
   - `Local\NAI.Lattice.Host.Manager.v1` — cross-product mutex, held per
     install/launch decision, not for the process lifetime.
   - **Newest valid wins. Never downgrade.** Adopt a newer compatible host;
     degrade if it speaks a newer protocol than your product supports.
   - Spawn the host directly; no CLI subcommands. Stop only for a forward
     update, monitor before worker.
5. **Reference implementation:** in the NAI OS repository,
   `src/background/obs/lattice_host.rs` (runtime manager) and
   `scripts/ensure-fabric-host.ps1` (production staging wrapper). Port the
   decision logic; do not invent a second interpretation of §2–§7 of the
   contract.
6. **Verify with the checklist** in `docs/PRODUCT_BUNDLING.md` §10, including a
   mixed-version test with ~10 products and a random start order: one serving
   worker, highest bundled version running, no downgrade, no flapping.

## If you are working on NAI-Lattice itself

- Keep the producer side of the contract stable:
  - binary name `nai-fabric-host.exe`;
  - `endpoint.json` fields consumed by products (`pid`, `protocol`,
    `canonical_pipe`, `control_http_port`, `instance_id`,
    `published_at_unix_ms`);
  - the per-user singleton;
  - the NAF handshake range. Advance `protocol_min` only with an intentional
    breaking change; older products will degrade until updated.
- Build: `cargo build --release -p nai-fabric-host`.
- Tests: `cargo test`.
- Version numbers are monotonic and immutable; never reuse a version for
  different bytes.
