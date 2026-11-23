# Security

## Supported versions

Only the latest published release is supported.

## Reporting a vulnerability

Use GitHub's private vulnerability reporting on this repository, or email
`satin@nanotrik.cz`. Please do not open a public issue for a security report.

## Code signing and trust anchor

NAI Lattice Windows binaries (`nai-fabric-host.exe`, `nai-lattice-cli.exe`) are
Authenticode-signed with the NAI sovereign code-signing chain. The public
certificates are committed in [`certs/`](certs/); the private keys are not in
this repository.

This is a **private sovereign CA**: the trust anchor is our own root, not a
public or commercial CA. "Authenticode" here is the Windows signature format
that carries the leaf signature; it does not imply a public CA.

| Role | Subject | SHA-1 thumbprint | Validity |
|---|---|---|---|
| Root CA | `CN=NAI Sovereign Root CA, C=US` | `A9144C0CDBC440135DB7E95480DF374D52D565EA` | 2026-11-22 → 2046-11-22 |
| Intermediate | `CN=NAI Code Signing CA, C=US` | `B6A32419C87FB0DA104EA815C117450A75027532` | 2026-11-22 → 2036-11-22 |
| NAI Lattice leaf | `CN=NAI Lattice, C=US` | `9C6CBCF11016FD17ABCA7F7816A16E7ECBDE861D` | 2026-11-22 → 2029-11-22 |
| NAI OS leaf | `CN=NAI OS` | `1FA3E6A536352A37342742CEBED4CA546944F5F7` | 2026-11-23 → 2029-11-23 |

The NAI OS leaf uses the exact subject `CN=NAI OS` (it must match
the MSIX `Publisher`) and is issued from the same intermediate. The old
self-signed development leaf (`E038F41B…`) is retired; the next NAI OS build
uses the sovereign leaf.

The private authority (root/intermediate keys + issuance runbook) is the
internal `<private-ca-authority>` workspace. The public trust index is
`<release-host>/NAI-CA/latest.json`; the public certificates here
are byte-identical copies.

### Verify a download

```powershell
Get-AuthenticodeSignature .\nai-fabric-host.exe |
  Format-List Status, SignerCertificate

signtool verify /pa /all /v .\nai-fabric-host.exe
```

`Status` must be `Valid` and the signer must be the NAI Lattice leaf above.
Every release also ships a SHA-256 sidecar; compare it before running:

```powershell
Get-FileHash .\nai-fabric-host.exe -Algorithm SHA256
```

### Install the trust anchor (elevated)

```powershell
Import-Certificate -FilePath certs\NAI-Sovereign-Root-CA.cer -CertStoreLocation Cert:\LocalMachine\Root
Import-Certificate -FilePath certs\NAI-Code-Signing-CA.cer   -CertStoreLocation Cert:\LocalMachine\CA
```

The installer does the same two imports. A root rotation ships a new root and
removes the previous thumbprint; never keep two NAI roots trusted at once.

### Signing policy

- The root private key is kept offline. The release box holds only the
  intermediate and the product leaf.
- Every release signature is timestamped; a signature remains verifiable after
  the leaf expires.
- Release binaries are signed **before** the host manifest is written, so the
  manifest's `sha256` and `signer_thumbprint` describe the exact signed bytes.
