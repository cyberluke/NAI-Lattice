# NAI OS — first production integration

![NAI OS](../assets/nai-os-icon.png)

**[NAI OS](https://github.com/cyberluke/NAI-OS)** is the first production-ready
integration of NAI Lattice. It is the reference consumer of the fabric host and
the workspace's production validation.

> The NAI OS source repository is currently private and is being prepared for
> public release. The link above is the canonical destination.

## What NAI OS ships

- **`nai-fabric-host` as the shared per-user runtime.** NAI OS stages the host
  under `%LOCALAPPDATA%\NAI\Lattice\runtime\<version>`, maintains `active.json`,
  and spawns the host directly.
- **A bundled host manifest.** `host-manifest.json` carries the version, NAF
  protocol range, SHA-256, `signature_status`, `signer_subject` and
  `signer_thumbprint`.
- **Verify-before-execute.** The consumer checks SHA-256 **and** the Authenticode
  signer thumbprint before running the host.
- **Shared durable state.** NAI OS consumes the same canonical ledger, CAS, AAF
  and observability plane for its AI apps, N-Pilot and shell surfaces.

The producer contract is
[`docs/PRODUCT_BUNDLING.md`](PRODUCT_BUNDLING.md); signing and trust are in
[`SECURITY.md`](../SECURITY.md).

## Why it matters

NAI Lattice is not a demo crate. It is the runtime that a shipping desktop
product installs, verifies, supervises and upgrades. NAI OS exercises that full
path: staging, activation, live release swap, crash recovery and signed-host
verification.

## Links

- NAI OS repository: <https://github.com/cyberluke/NAI-OS>
- NAI Lattice repository: <https://github.com/cyberluke/NAI-Lattice>
- Product bundling contract: [`PRODUCT_BUNDLING.md`](PRODUCT_BUNDLING.md)
- Security and signing: [`SECURITY.md`](../SECURITY.md)
