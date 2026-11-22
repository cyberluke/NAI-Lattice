# Product bundling: shipping the NAI-Lattice fabric host

**Audience:** coding agents and build engineers working in any NAI product repo.
**Status:** canonical contract for every product that ships the fabric host.
**Reference implementation:** `<NAI-OS>/src/background/obs/lattice_host.rs`
(Rust/Tauri) and `<NAI-OS>/scripts/ensure-fabric-host.ps1`.
**Last updated:** 2026-11-21.

> If this document and an implementation disagree, treat the host binary's
> observed behaviour plus the reference implementation as authoritative, then
> update this document in the same change.

---

## 1. What you are shipping

`nai-fabric-host.exe` is a **shared per-user runtime**, not a per-product
service. Think VC++ redistributable:

- Every NAI product **bundles its own copy** (plus a manifest) in its installer.
- Exactly **one serving host** exists per user session. The host enforces this
  with its own per-user singleton; the product must not try to run a second one.
- The host owns the fabric pipe, the ledger/hotstore, and
  `%LOCALAPPDATA%\NAI\Lattice\v1\endpoint.json`.
- The product owns only the **lifecycle**: verify the bundled copy, install it
  into the shared runtime root, start it if absent, and update it forward.

**Do not depend on the optional HELLDESK release tooling** (`helldesk-release`,
`releases\<version>`, `current.json`). It is not part of the product contract and
may be absent. Product lifecycles must never invoke it or write those paths.

---

## 2. Golden rules

1. **One shared install root**, never a product-specific directory.
2. `active.json` is the only shared version pointer. Write it **last**, under the
   cross-product mutex, atomically.
3. **Never downgrade.** Newest valid wins. A newer compatible running host is
   adopted.
4. **Verify before execute.** SHA-256 must match the manifest. When the manifest
   carries `signer_thumbprint`, the bundled exe's Authenticode signer must match
   it and `signature_status` must be `Valid`; the version label must be a safe
   path component.
5. **Spawn the host directly.** No `helldesk-release` subcommands, no wrapper
   script requirements.
6. **Stop only for a forward upgrade.** Verify the image leaf is exactly
   `nai-fabric-host.exe`; stop the monitor before its worker.
7. **Protocol compatibility** comes from `endpoint.json`. If a newer running
   host is outside your supported range, degrade — do **not** kill it.
8. **Never delete the shared runtime**, including on uninstall. Another product
   may be using it.
9. **Version numbers are monotonic and immutable.** Never reuse a version for
   different bytes. Rollback = ship a higher version; a recovery downgrade is
   allowed only for an invalid active artifact (see §8).
10. **Legacy layouts are read-only.** `releases\<version>` and `v1\nai-fabric-host.exe`
    may be observed for adoption/migration; never write them.

---

## 3. Canonical paths and schemas

### Paths (per user)

| Path | Owner | Purpose |
|---|---|---|
| `%LOCALAPPDATA%\NAI\Lattice\active.json` | **products (shared)** | Active release pointer |
| `%LOCALAPPDATA%\NAI\Lattice\runtime\<version>\nai-fabric-host.exe` | **products (shared)** | Installed host artifacts |
| `%LOCALAPPDATA%\NAI\Lattice\v1\endpoint.json` | host | Live endpoint manifest (read-only) |
| `%LOCALAPPDATA%\NAI\Lattice\releases\...`, `v1\nai-fabric-host.exe` | optional HELLDESK / legacy | Migration/adoption only |

### Bundled manifest (`host-manifest.json`, inside your package)

```json
{
  "version": "0.3.49",
  "protocol_min": 1,
  "protocol_max": 1,
  "target": "x86_64-pc-windows-msvc",
  "sha256": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
  "built_at_unix_ms": 1760000000000,
  "source": "NAI-Lattice",
  "git": "1a2b3c4",
  "signature_status": "Valid",
  "signer_subject": "CN=NAI Lattice",
  "signer_thumbprint": "0123456789abcdef0123456789abcdef01234567"
}
```

- `version`: the `nai-fabric-host` package version (from `cargo metadata`).
- `protocol_min`/`protocol_max`: the NAF handshake range **this product**
  supports (currently `1..1`; bump only with a deliberate break).
- `target`: the Windows target triple that produced the exe.
- `sha256`: lowercase 64-char hex of the bundled exe.
- `source`/`git`/`built_at_unix_ms`: audit metadata.
- `signature_status`: Authenticode status at packaging time (`Valid`,
  `NotSigned`, ...). Release bundles must be `Valid`.
- `signer_subject`/`signer_thumbprint`: present when the exe is signed. A
  consumer that enforces signing pins the NAI Lattice leaf thumbprint and
  rejects any other signer or a `NotSigned` bundle.

### Shared activation (`active.json`)

```json
{
  "version": "0.3.49",
  "sha256": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
  "source": "NAI-Lattice",
  "git": "1a2b3c4",
  "installed_at_unix_ms": 1760000000000
}
```

- Written only by the product that activates a release.
- `sha256` must be the SHA-256 of `runtime\<version>\nai-fabric-host.exe`.
- Read by every product as the shared desired state.
- Write atomically: `active.json.tmp-<pid>` → `rename` over `active.json`.

### Host endpoint (`v1\endpoint.json`, written by the host)

Read-only for products. Relevant fields:

```json
{
  "pid": 1234,
  "canonical_pipe": "\\\\.\\pipe\\NAI.AmbientFabric.NAF1.<sid-hash>",
  "control_http_port": 8787,
  "protocol": { "min": 1, "max": 1, "selected": 1 },
  "instance_id": "...",
  "published_at_unix_ms": 1760000000000
}
```

- `pid` is the serving worker (not necessarily the monitor).
- Determine the running version from the process image path:
  `...\Lattice\runtime\<version>\nai-fabric-host.exe` (or legacy
  `...\Lattice\releases\<version>\...`).
- `protocol.selected` must be inside your `[protocol_min, protocol_max]`.

---

## 4. Build-time bundling

### 4.1 Build steps

1. Resolve the host version from the NAI-Lattice workspace:
   `cargo metadata --no-deps --format-version 1 --manifest-path <lattice>/Cargo.toml`
   → package `nai-fabric-host` → `version`.
2. Build for the product's Windows target:
   `cargo build --release -p nai-fabric-host --target x86_64-pc-windows-msvc`.
   If you set release-profile overrides (`CARGO_PROFILE_RELEASE_*`) for a fast
   build, **save and restore them** so you do not silently change the parent
   product's LTO/codegen settings.
3. Copy the exe and write `host-manifest.json` into your package resources.
4. **Stage before** the build step that hashes or copies resources. In NAI OS,
   `build.rs` writes `SHA256SUMS` and the MSIX bundler copies `src/static`; the
   host must exist before both run.
5. **Fail the release build** if the host is missing. Never warn-and-skip: a
   host-less installer looks healthy and silently loses observability.
6. **Gitignore** the staged host directory; it is a build artifact.

### 4.2 Canonical script (use this)

```powershell
pwsh NAI-Lattice/packaging\bundle-host-for-product.ps1 `
     -OutDir <product>\resources\lattice\host `
     -Target x86_64-pc-windows-msvc
```

The script resolves the host package version from `cargo metadata`, builds
`nai-fabric-host` with scoped release-profile overrides, copies the exe, writes
the manifest with a verified SHA-256, and fails hard on any error.

| Parameter | Default | Meaning |
|---|---|---|
| `-OutDir` | required | product resource directory that receives the exe + manifest |
| `-Target` | `x86_64-pc-windows-msvc` | cargo target triple |
| `-Lattice` | parent of `packaging\` | NAI-Lattice workspace |
| `-ProtocolMin` / `-ProtocolMax` | `1` / `1` | NAF range this product supports |

Call it from the product's release pipeline **before** resource hashing/copying,
and fail the release if it fails. Do not commit `-OutDir`; gitignore it.

---

## 5. Runtime lifecycle (language-neutral)

Run this at product startup and periodically (a watchdog tick is fine). Acquire
the shared mutex per turn, never for the process lifetime.

```text
ensure():
  bundle = read <package>/lattice/host/host-manifest.json
  verify bundle.version is a safe path label (no / \ : .., no whitespace)
  verify bundle.sha256 == sha256(bundle.exe)
  verify bundle.target == this product's target triple

  lock = open mutex "Local\\NAI.Lattice.Host.Manager.v1"
  if wait(lock, 10s) != acquired: return SKIP        // another product is managing
  try:
    active = read %LOCALAPPDATA%\NAI\Lattice\active.json
    activeValid =
        active != null
        && runtime\<active.version>\nai-fabric-host.exe exists
        && sha256(artifact) == (active.version == bundle.version
                                ? bundle.sha256 : active.sha256)

    desired = newer(bundle.version, active.version if activeValid)

    ep = read %LOCALAPPDATA%\NAI\Lattice\v1\endpoint.json
    running = (ep.pid alive && imageLeaf(ep.pid) == "nai-fabric-host.exe")
                ? versionFromPath(imagePath(ep.pid))   // runtime\<v> or releases\<v>
                : null
    protocolOk = ep.protocol.selected in [bundle.protocol_min, bundle.protocol_max]

    if running != null && protocolOk && cmp(running, desired) >= 0:
        return HEALTHY or ADOPTED(running)

    if running != null && !protocolOk && cmp(running, desired) >= 0:
        return DEGRADED("this product is older than the running host's protocol")
        // never kill a newer shared host

    if desired == bundle.version && !(active.version == desired && activeValid):
        if active != null && cmp(active.version, desired) > 0 && activeValid:
            return ADOPTED(active.version)             // never downgrade
        install(bundle)                                // see below

    if running != desired:
        if running != null:
            stopHostTree()                             // forward-only
        spawn(runtime\<desired>\nai-fabric-host.exe)   // detached, no console
        waitUntil(ep.pid alive && versionFromPath == desired, 30s)
  finally:
    release(lock)
```

### Install (atomic, never downgrade)

1. If `active.version == bundle.version` and its artifact hashes, return.
2. If `active.version` is **newer and valid**, return (adopt; do not install).
3. Copy `bundle.exe` → `runtime\<version>\nai-fabric-host.exe.tmp-<pid>`.
4. Verify the copied SHA-256 against the manifest; delete the temp on mismatch.
5. Rename over `runtime\<version>\nai-fabric-host.exe`.
6. Re-read `active.json`; if a **newer valid** activation appeared, keep it and
   return without writing.
7. Write `active.json` atomically (version, sha256, source, git, timestamp).

Only a **newer valid** active blocks the install. A newer **invalid** active is
broken shared state and may be recovered by installing the desired bundle; log
it as a recovery.

### Launch

- Spawn the installed exe directly:
  - Windows: `CREATE_NO_WINDOW` (`0x08000000`), null stdin/stdout/stderr,
    detached (do not wait for exit).
- No arguments. The host runs as its monitor/worker pair and enforces the
  per-user singleton itself.
- Do not call `helldesk-release launch` or any CLI subcommand.

### Stop (forward upgrade only)

- Determine `worker = endpoint.pid` if it is alive and its image leaf is
  `nai-fabric-host.exe`.
- Determine `monitor` = a host process (excluding the worker) whose parent is
  **not** another host process.
- Terminate the **monitor first**, wait for exit, then the worker. This prevents
  the monitor from respawning the old worker during the swap.
- Re-check liveness before each termination; a monitor's Job Object may already
  have killed the worker.
- Never stop a host whose version is `>= desired`. Adopt it instead.

---

## 6. Version and protocol policy

### Comparison

Use a semver-ish numeric comparison, not string comparison:

- `0.3.49 < 0.3.50`, `0.4.0 > 0.3.99`, `1.0.0 > 0.9.9`.
- `v1.2.3` and `1.2.3` are equal.
- A release (`1.0.0`) is newer than a pre-release (`1.0.0-rc1`).

### Decision matrix

| Running host | Protocol | Action |
|---|---|---|
| absent | — | install desired if needed, spawn desired |
| `>= desired` | compatible | **adopt/healthy** (skip) |
| `>= desired` | incompatible | **degraded**: product is too old; do not touch |
| `< desired` | either | install desired if needed, forward stop + spawn desired |
| unversioned legacy (`v1`) | compatible | adopt (no version management) |
| unversioned legacy (`v1`) | incompatible | degraded; do not kill |

`desired = max(bundled.version, active.version if activeValid)`.

### Protocol range

- The product's manifest declares `protocol_min..protocol_max`.
- `endpoint.json.protocol.selected` must fall inside it.
- A newer host with an out-of-range protocol means **this product must update**;
  do not downgrade the host and do not kill it.
- Advance the protocol range only with intentional planning; older products will
  degrade until updated.

---

## 7. Ten products at runtime without clashes

### Why it cannot clash

- **One shared mutex** (`Local\NAI.Lattice.Host.Manager.v1`) serializes every
  install/activate/stop/spawn decision.
- **`active.json` is written last and atomically**, so no partial activation is
  ever observable.
- **Newest valid wins; never downgrade.** A product with an older bundled host
  cannot replace a newer active/running host.
- **The host enforces its own per-user singleton**, so even an uncoordinated
  duplicate spawn cannot create a second serving worker.
- **Every ensure is idempotent**: same version + valid artifact + running worker
  = no filesystem or process churn.
- **Forward-only stops**: only a product with a newer desired version stops the
  running tree.

### Recommended process

1. Each product bundles a host version (they do **not** have to match).
2. At startup and on the watchdog tick, each product runs `ensure()`.
3. The first product to take the mutex decides; the rest either adopt or wait.
4. When a product ships a newer host:
   - It installs atomically, updates `active.json`, stops the old tree, spawns
     the new one, waits for `endpoint.pid` to report the new version.
   - All other products see `running >= desired` and adopt; one swap total.
5. When an older product starts later:
   - It reads the newer `active.json`, verifies the artifact, sees the newer
     running host, and adopts. No install, no downgrade.
6. If a newer host is **incompatible** with an older product's protocol:
   - The older product reports degraded and asks the user to update it.
   - The newer host keeps serving the newer products.

### Example (10 products, different bundle versions)

Bundled versions: `A=0.3.40`, `B=0.3.45`, `C=0.3.49`, `D=0.3.49`, `E=0.3.52`,
`F=0.3.52`, `G=0.3.47`, `H=0.3.50`, `I=0.3.44`, `J=0.3.52`.

- Start in random order.
- The first `ensure()` from `E/F/J` (or any 0.3.52 product) installs **0.3.52**
  and spawns it.
- Every other product adopts 0.3.52 (protocol compatible) and performs no
  writes.
- If only older products start first, the newest among them runs (e.g. 0.3.50);
  when a 0.3.52 product starts, it performs **one** forward upgrade, then all
  adopt.
- No product ever installs 0.3.40 over 0.3.52; no flapping.

### Startup storm

Products may all tick at once. Only one acquires the mutex; the rest skip and
retry. The host singleton is a second safety net. Expected steady state:
**one serving worker** (one `endpoint.json.pid`); a monitor process may exist as
its parent. Do not assert "one process total" — assert one serving worker.

### Uninstall / cleanup

- Never delete `active.json` or `runtime\` on product uninstall; other products
  may be using them.
- A product may remove only its own packaged copy.
- If a shared-runtime garbage collector is ever added, it must be
  reference-counted across installed products and must never remove the active
  release.

---

## 8. Failure, crash and edge cases

| Situation | Required behaviour |
|---|---|
| Bundle manifest missing/invalid | Do not execute the bundle. Log the exact path. If a compatible shared host is already running, adopt it; otherwise degrade. |
| Bundle SHA-256 mismatch | Refuse to install/execute; treat the package as corrupt. |
| `active.json` missing | Install the bundled release; write `active.json`. |
| Active artifact missing/corrupt | If desired == bundled, reinstall atomically. If a newer active is invalid, recovery-install the bundled version and log it. |
| Newer valid active installed by another product | Adopt/launch it; never install your older bundle. |
| Newer invalid active | Recovery-downgrade to your bundle only if no compatible newer host is running; log `recovery`. |
| Host crashed | Watchdog restores it with backoff. One product's ensure wins; the rest adopt. |
| Repeated crashes | Backoff and a crash-loop guard (reference: 5 controlled restarts / 10 min). Do not spawn/kill in a tight loop. |
| Monitor alive, worker absent | Wait a bounded grace period (reference: 60 s); if no desired worker appears, stop the tree and relaunch. |
| Transient file read denied (AV/indexer) | Retry the hash once; do not classify a single read error as corruption. |
| Disk full / permission denied | Leave any running host alone; report the error and back off. |
| MSIX/package update | The package path changes; re-resolve the manifest each run. `active.json`/`runtime` persist. |
| Product runs elevated | The spawned host inherits the token. Prefer spawning at the user's integrity level; a network-facing host should not run elevated. |
| Multiple Windows sessions (RDP) | The manager mutex is per session. Current contract assumes one interactive session per user. If you support multi-session, add a **cross-session exclusive file lock** (e.g. `%LOCALAPPDATA%\NAI\Lattice\active.lock`) around install/activation and launch. |
| Optional HELLDESK `releases\current.json` present | Legacy/migration only. Do **not** write or delete it from a product. Log a warning; the host monitor may resolve that layout. Remove it only through the HELLDESK owner's process. |
| Uninstall | Never delete the shared runtime. |
| Panic in the supervisor | Release the manager mutex, log, continue; do not silently kill the supervisor thread. |

### Rollback

- Normal rollback = ship a **higher** version containing the fix.
- A recovery downgrade is permitted only when the active artifact fails
  verification and no compatible newer host is running.
- Never reuse a version number for different bytes; the shared pointer and other
  products would disagree about what that version means.

---

## 9. Migration from the optional HELLDESK layout

- If a `releases\<version>\nai-fabric-host.exe` host is running, parse its
  version from the image path and treat it exactly like a `runtime\<version>`
  host: adopt if `>= desired` and compatible, otherwise forward-upgrade.
- Do not read or write `releases\current.json` for your own activation.
- Do not delete the HELLDESK layout; another tool may own it.
- The old `v1\nai-fabric-host.exe` staging path is unversioned: adopt it if
  compatible, otherwise degrade; never kill it just to replace it.

---

## 10. Verification checklist (must pass before shipping)

Run these with 10 product builds (or 10 copies with different bundled versions):

- [ ] Random start order: exactly **one serving worker** (`endpoint.json.pid`),
      and it runs the **highest bundled version** among started products.
- [ ] Start an older product after the newest: it adopts, performs no install,
      no downgrade, no stop.
- [ ] Start a newer product: exactly one forward swap; all products end on the
      newer version.
- [ ] Kill the host: exactly one product restores it; no duplicate workers.
- [ ] Corrupt the active artifact: one product repairs it; the rest defer.
- [ ] Running host newer + protocol-incompatible: older products log degraded
      and do **not** terminate the host.
- [ ] No `helldesk-release` process is ever created by a product.
- [ ] No product writes `releases\` or `current.json`.
- [ ] `endpoint.json.protocol.selected` is inside every participating product's
      declared range.
- [ ] A repeated `ensure()` in the steady state performs zero file writes and
      zero process spawns.

Useful checks:

```powershell
$ep = Get-Content "$env:LOCALAPPDATA\NAI\Lattice\v1\endpoint.json" -Raw | ConvertFrom-Json
$active = Get-Content "$env:LOCALAPPDATA\NAI\Lattice\active.json" -Raw | ConvertFrom-Json
"worker pid=$($ep.pid) protocol=$($ep.protocol.selected) active=$($active.version)"
Get-CimInstance Win32_Process -Filter "Name='nai-fabric-host.exe'" |
  Select-Object ProcessId, ParentProcessId, ExecutablePath
```

Expected: one serving worker pid; optionally its monitor as the parent. All
product supervisors should report the same `active.version`.

---

## 11. Reference implementation

- **Runtime manager:** `<NAI-OS>/src/background/obs/lattice_host.rs`
  - Adopt/never-downgrade policy, manager mutex, atomic `runtime\` install,
    direct spawn, forward-only stop, watchdog, crash-loop guard, panic isolation.
- **Canonical staging script (all products):**
  `NAI-Lattice/packaging\bundle-host-for-product.ps1`
  - Version from `cargo metadata`, scoped profile overrides, SHA-256 manifest,
    fail-hard.
- **NAI OS production wrapper:** `<NAI-OS>/scripts/ensure-fabric-host.ps1`
  - `-Stage Package -RequireHost`, stage modes, atomic manifest writes.
- **Release ordering:** `<NAI-OS>/scripts\build-release-local.ps1`
  - Host staged **before** `cargo build` so resource hashing and the MSIX bundle
    include it.
- **Host itself:** `NAI-Lattice/apps\nai-fabric-host\src\main.rs`
  (monitor/child, singleton, endpoint publication) and `release.rs` (optional
  HELLDESK tooling — not used by the product contract).

Port the decision logic; do not invent a second translation of "newest wins".
If your product's language makes the reference impossible to port directly,
implement the contract in §2–§7 and add a conformance test for §10.

---

## 12. Do / Don't

**Do**

- Bundle, verify, install atomically, spawn directly, watch, adopt newer.
- Take the shared mutex per turn; write `active.json` last.
- Keep version numbers monotonic; treat a version as immutable bytes.
- Leave the shared runtime in place on uninstall.
- Log actionable codes (`LATTICE_HOST_*`-style) for every failure.

**Don't**

- Don't use `helldesk-release`, `releases\`, or `current.json` in a product
  lifecycle.
- Don't install under a product-specific directory.
- Don't spawn without the shared mutex (even though the host singleton protects
  you, uncoordinated spawns cause churn).
- Don't downgrade a newer host, even if your bundle is "known good".
- Don't kill a newer host that speaks a newer protocol; degrade instead.
- Don't hold the manager mutex for the process lifetime.
- Don't delete `active.json`/`runtime` on uninstall.
- Don't reuse a version number for different bytes.
