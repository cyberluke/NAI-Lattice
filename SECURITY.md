# Security

## Supported versions

Only the latest published release is supported.

## Reporting a vulnerability

Use GitHub's private vulnerability reporting on this repository, or email
`support@nanotrik.ai`. Please do not open a public issue for a security report.

## Code signing and trust anchor

NAI Lattice Windows binaries (`nai-fabric-host.exe`, `nai-lattice-cli.exe`) are
Authenticode-signed with the NAI sovereign code-signing chain. The public
certificates are committed in [`certs/`](certs/); the private keys are not in
this repository.

| Role | Subject | SHA-1 thumbprint | Validity |
|---|---|---|---|
| Root CA | `CN=NAI Sovereign Root CA, C=US` | `A9144C0CDBC440135DB7E95480DF374D52D565EA` | 2026-11-22 → 2046-11-22 |
| Intermediate | `CN=NAI Code Signing CA, C=US` | `B6A32419C87FB0DA104EA815C117450A75027532` | 2026-11-22 → 2036-11-22 |
| NAI Lattice leaf | `CN=NAI Lattice, C=US` | `9C6CBCF11016FD17ABCA7F7816A16E7ECBDE861D` | 2026-11-22 → 2029-11-22 |

The NAI OS leaf uses the exact subject `CN=NAI OS` (it must match
the MSIX `Publisher`). It is issued from the same intermediate; the old
self-signed development certificate is being retired in a coordinated NAI OS
change.

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
