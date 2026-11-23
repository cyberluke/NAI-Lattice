# NAI OS

<img src="../assets/nai-os-logo.png" alt="NAI OS" width="420" />

[NAI OS](https://github.com/cyberluke/NAI-OS) is the first production product
built on NAI Lattice.

## Runtime integration

- `nai-fabric-host` runs as the per-user runtime.
- `host-manifest.json` records the version, NAF protocol range and SHA-256 that
  the installer checks before launch.
- The host owns the canonical ledger, CAS, AAF and observability log.

The manifest also carries `signature_status`, `signer_subject` and
`signer_thumbprint`; wiring the signer check into the NAI OS host consumer is
the remaining integration step.

## References

- Host install/activation: [PRODUCT_BUNDLING.md](PRODUCT_BUNDLING.md)
- Signing and trust: [SECURITY.md](../SECURITY.md)
- Repository: <https://github.com/cyberluke/NAI-OS>
