# Releasing NAI Lattice — manual, local, no CI/CD

**This is the canonical release runbook for NAI Lattice.** The public GitHub
repository is **source-only**: no `.github/workflows`, no GitHub Actions, no
GitHub Releases, no CI/CD. Every release is a local sequence run by an operator
on the release box. Artifacts are published to the release host and the language
registries.

> If this document and a script disagree, fix the script in the same change.
> The contract for products that ship the fabric host is
> [`PRODUCT_BUNDLING.md`](PRODUCT_BUNDLING.md); this document covers the
> producer side: signing, packaging and publishing.

---

## 0. Ground rules

1. **One private root, one intermediate, per-product leaves.** The root and
   intermediate are operated privately by the NAI CA authority. This repository
   consumes only the issued product leaf; the public certificates are
   distributed with the release.
2. **Never commit a private key.** PFX files, passwords and the root key live
   outside git (`.cert/*.pfx*` is ignored).
3. **Sign before packaging.** The host manifest records the signed bytes; any
   re-sign invalidates the manifest. Timestamp every signature so it survives
   leaf expiry.
4. **Versions are immutable.** Never reuse a version for different bytes.
   Rollback = publish a higher version.
5. **Namespace artifact keys.** NAI Lattice writes `nai-lattice/...` and
   `nai-lattice/latest.json`. Never write the registry-root `latest.json` (that
   belongs to another product).
6. **No workflow files.** If a change adds `.github/workflows`, it is out of
   contract.

---

## 1. The signing chain

Release binaries are signed with a short-lived product leaf issued by the
private NAI CA authority. Root and intermediate key material, and the issuance
runbook, are not part of this repository.

| Level | Subject | Life | Constraints | Where the key lives |
|---|---|---|---|---|
| Root | `CN=NAI Sovereign Root CA` | 20y | `CA=true, pathlen:1`, `keyCertSign,cRLSign` | offline machine / removable media |
| Intermediate | `CN=NAI Code Signing CA` | 10y | `CA=true, pathlen:0` | release box, password-protected PFX |
| Leaf (NAI Lattice) | `CN=NAI Lattice` | 3y | `CA=false`, EKU `codeSigning` | `.cert/NAILattice.pfx` (gitignored) |

Other product leaves are issued and rotated by the private CA authority; they
are not part of this release runbook.

### 1.1 Sign and verify

```powershell
pwsh ./scripts/release/sign-release.ps1 `
  -Files target\x86_64-pc-windows-msvc\release\nai-fabric-host.exe,
         target\x86_64-pc-windows-msvc\release\nai-lattice-cli.exe

Get-AuthenticodeSignature target\x86_64-pc-windows-msvc\release\nai-fabric-host.exe |
  Format-List Status, SignerCertificate
```

The script runs `signtool sign /fd SHA256 /tr <RFC3161> /td SHA256`, then
`signtool verify /pa /all /v`, then requires `Get-AuthenticodeSignature` to be
`Valid`. A timestamp is mandatory for releases.

### 1.2 Trust provisioning (installer / first run, elevated)

```powershell
Import-Certificate -FilePath .cert\NAI-Sovereign-Root-CA.cer -CertStoreLocation Cert:\LocalMachine\Root
Import-Certificate -FilePath .cert\NAI-Code-Signing-CA.cer   -CertStoreLocation Cert:\LocalMachine\CA

# MSIX sideload only:
Import-Certificate -FilePath .cert\NAIOS.cer -CertStoreLocation Cert:\LocalMachine\TrustedPeople
```

Publish the root `.cer` and its SHA-256 through the public trust index and in
both repositories. Every installer installs the same root; a root rotation ships
a new root and removes the old thumbprint.

---

## 2. Build

```powershell
# Bump every manifest first (workspace + both npm packages)
pwsh ./scripts/release/bump-version.ps1 `
  -CargoVersion 0.1.1 `
  -NpmClientVersion 0.2.1 `
  -NpmMcpVersion 0.2.0

# Rust release binaries (default host target; no --target triple)
cargo build --release -p nai-fabric-host -p nai-lattice-cli
# -> target\release\nai-fabric-host.exe and target\release\nai-lattice-cli.exe

# TypeScript packages
npm ci
npm run build:ts
```

The host binary name is fixed by the product contract:
`nai-fabric-host.exe`. Do not rename it.

---

## 3. Sign, then package the host

Sign the exact bytes first; the manifest must describe the signed binary.

```powershell
$hostExe = 'target\release\nai-fabric-host.exe'
$cliExe  = 'target\release\nai-lattice-cli.exe'

pwsh ./scripts/release/sign-release.ps1 `
  -Files $hostExe,$cliExe `
  -TrustChain

pwsh ./packaging/bundle-host-for-product.ps1 `
  -OutDir dist\nai-lattice\host `
  -SkipBuild `
  -HostExe $hostExe `
  -RequireSignature
```

`-TrustChain` installs the sovereign root/intermediate into the **current
user's** stores so `signtool verify` and `Get-AuthenticodeSignature` can build
the chain on the release box. The installer does the machine-wide equivalent.

`-SkipBuild` is mandatory here: without it the bundler would rebuild and
replace the signed binary. It copies the signed host, reads its Authenticode
status, and writes `host-manifest.json` with `sha256`, `signature_status`,
`signer_subject` and `signer_thumbprint`. `-RequireSignature` fails the bundle
when the exe is not Authenticode-`Valid`.

Products consume the manifest per [`PRODUCT_BUNDLING.md`](PRODUCT_BUNDLING.md):
verify SHA-256 **and** the signer thumbprint before executing.

---

## 4. Publish release artifacts

```powershell
. ./scripts/release/load-env.ps1   # R2_* keys and endpoint from .env.local

pwsh ./scripts/release/r2-release.ps1 `
  -Version 0.1.1 `
  -Artifacts dist\nai-lattice\host\nai-fabric-host.exe,
             target\release\nai-lattice-cli.exe,
             dist\nai-lattice\host\host-manifest.json,
             <private-certs>\NAI-Sovereign-Root-CA.cer,
             <private-certs>\NAI-Code-Signing-CA.cer `
  -Latest
```

Stage the public root and intermediate certificates from the private CA
authority into `<private-certs>`; they are release artifacts, not source files.

The script consumes `R2_BUCKET`, `R2_SERVER`, `R2_ACCESS_KEY`,
`R2_SECRET_KEY`, `R2_FOLDER_PREFIX` and `R2_PUBLIC_URL` directly (the
`CLOUDFLARE_R2_*` / `AWS_*` names still work). `R2_PUBLIC_URL` may include the
product prefix; the script strips it so the generated URI is never doubled.

Layout (`R2_FOLDER_PREFIX=NAI-Lattice/`):

```
s3://<bucket>/NAI-Lattice/0.1.0/<asset>
s3://<bucket>/NAI-Lattice/0.1.0/<asset>.sha256
s3://<bucket>/NAI-Lattice/latest.json

<release-host>/NAI-Lattice/0.1.0/<asset>
<release-host>/NAI-Lattice/latest.json
```

`latest.json` is the only mutable object. Versioned objects are immutable and
cached for a year.

---

## 5. Publish to npm

```powershell
npm login                    # once per machine
pwsh ./scripts/release/publish-npm.ps1              # dry run
pwsh ./scripts/release/publish-npm.ps1 -Execute
```

Order is fixed: `@cyberluke/nai-lattice-client` first, then
`@cyberluke/mcp-nai-lattice` (it depends on the client). `prepublishOnly`
rebuilds each package. There is no OIDC provenance because there is no CI.

---

## 6. Publish to crates.io

```powershell
cargo login                  # once per machine
pwsh ./scripts/release/publish-crates.ps1            # dry run: prints order
pwsh ./scripts/release/publish-crates.ps1 -Execute
```

The script publishes in dependency order, verifies that every intra-workspace
dependency version matches the workspace version, and retries while the
crates.io index propagates.

- Apps (`nai-fabric-host`, `nai-lattice-cli`) are `publish = false`; they ship
  as binaries, not registry crates.
- The crate **names** are `nai-lattice-*`; directory names may still read
  `crates/lattice-*` (cosmetic only).
- Cargo requires a literal `version = "x.y.z"` on every `path` dependency; when
  you bump the workspace version, bump those literals in the same change.
  `publish-crates.ps1` refuses to publish a mixed-version graph.

---

## 7. Public GitHub repository (source only)

The repository is public for source review and contribution. It is **not** a
release channel.

```powershell
# After the audit below:
gh repo edit cyberluke/NAI-Lattice --visibility public --accept-visibility-change-consequences
```

Pre-publication audit:

- [ ] No `.github/workflows`, no release assets, no Actions.
- [ ] Full-history secret scan (`gitleaks` / `trufflehog`), not just the working
      tree. `.env*`, `*.pfx`, `*.key` are gitignored.
- [ ] `README.md`, `AGENTS.md` and docs contain no internal absolute paths,
      private hostnames, private IP addresses, bucket names, or CA-issuance
      procedures.
- [ ] License is consistent: `LICENSE` (Apache-2.0) + `LICENSE-MIT`, and every
      manifest says `MIT OR Apache-2.0`.
- [ ] `packages/mcp-nai-lattice/projects.json` and local state are not tracked.
- [ ] `cargo check`, `cargo test`, `npm run build:ts`, `npm run test:ts` pass.

Tags are optional source markers; they do not create releases.

---

## 8. Sync with NAI OS

NAI OS consumes the published crates through dependency aliases, so its Rust
imports remain `lattice_*`:

```toml
lattice-client = { package = "nai-lattice-client", path = "../../NAI-Lattice/crates/lattice-client" }
lattice-protocol = { package = "nai-lattice-protocol", path = "../../NAI-Lattice/crates/lattice-protocol" }
lattice-observability = { package = "nai-lattice-observability", path = "../../NAI-Lattice/crates/lattice-observability" }
```

NAI OS verifies the host manifest before launch; its signer policy and leaf
rotation live in the NAI OS repository and the private CA authority.

---

## 9. Release checklist

- [ ] Version bumped in the Rust workspace and the two npm packages
      (`bump-version.ps1`), and `Cargo.lock` committed.
- [ ] `cargo check --workspace` passes.
- [ ] `cargo test --workspace` passes.
- [ ] `npm run build:ts` and `npm run test:ts` pass.
- [ ] Host and CLI signed; `signtool verify /pa /all` passes.
- [ ] `bundle-host-for-product.ps1 -RequireSignature` wrote a manifest whose
      `sha256` matches the signed exe.
- [ ] Versioned artifact URLs and `nai-lattice/latest.json` resolve.
- [ ] `npm view @cyberluke/nai-lattice-client version` shows the new version.
- [ ] `npm view @cyberluke/mcp-nai-lattice version` shows the new version.
- [ ] `cargo search nai-lattice-core` shows the new version.
- [ ] NAI OS starts against the published host and reports the expected version.
- [ ] Working tree is clean; the release commit is tagged (optional).
