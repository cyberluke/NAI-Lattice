# Copyright (c) NAI OS
# Licensed under the MIT License.
#
# Canonical build + stage step for a NAI product that ships the fabric host.
# Run this BEFORE the product's resource hashing/packaging step (e.g. before
# build.rs computes SHA256SUMS or the MSIX bundler copies resources).
#
# Contract + rationale: docs/PRODUCT_BUNDLING.md
#
# Produces in -OutDir:
#   nai-fabric-host.exe
#   host-manifest.json   (version, protocol range, target, SHA-256, source, git,
#                         signature_status + signer_subject/thumbprint)
#
# Fail-hard by design: a product release must never silently omit the host.
#
# Example:
#   pwsh .\packaging\bundle-host-for-product.ps1 `
#        -OutDir .\resources\lattice\host

param(
    [Parameter(Mandatory = $true)]
    [string]$OutDir,

    [ValidateSet('x86_64-pc-windows-msvc', 'aarch64-pc-windows-msvc')]
    [string]$Target = 'x86_64-pc-windows-msvc',

    [string]$Lattice = (Split-Path -Parent $PSScriptRoot),

    [int]$ProtocolMin = 1,
    [int]$ProtocolMax = 1,

    # Fail the bundle when the host is not Authenticode-Valid. Product release
    # builds set this; local/dev bundles may leave it off.
    [switch]$RequireSignature
)

$ErrorActionPreference = 'Stop'

if ($ProtocolMin -gt $ProtocolMax) {
    throw "bundle-host-for-product: ProtocolMin ($ProtocolMin) > ProtocolMax ($ProtocolMax)"
}

$workspace = Join-Path $Lattice 'Cargo.toml'
if (-not (Test-Path -LiteralPath $workspace)) {
    throw "bundle-host-for-product: NAI-Lattice workspace not found at $Lattice"
}

# ── version (single source of truth: nai-fabric-host package metadata) ───────
$metaJson = & cargo metadata --no-deps --format-version 1 --manifest-path $workspace 2>$null
if ($LASTEXITCODE -ne 0 -or -not $metaJson) {
    throw "bundle-host-for-product: cargo metadata failed for $workspace"
}
$meta = ($metaJson | Out-String) | ConvertFrom-Json
$package = $meta.packages | Where-Object { $_.name -eq 'nai-fabric-host' } | Select-Object -First 1
if (-not $package -or -not $package.version) {
    throw "bundle-host-for-product: cannot resolve nai-fabric-host version"
}
$version = [string]$package.version
$gitDesc = (& git -C $Lattice rev-parse --short HEAD 2>$null)
if (-not $gitDesc) { $gitDesc = 'unknown' }

# ── build (profile overrides scoped to this build only) ─────────────────────
$profileKeys = @(
    'CARGO_PROFILE_RELEASE_LTO',
    'CARGO_PROFILE_RELEASE_CODEGEN_UNITS',
    'CARGO_PROFILE_RELEASE_OPT_LEVEL',
    'CARGO_PROFILE_RELEASE_INCREMENTAL'
)
$savedProfile = @{}
foreach ($key in $profileKeys) {
    $savedProfile[$key] = [Environment]::GetEnvironmentVariable($key, 'Process')
}

try {
    if (-not $env:CARGO_PROFILE_RELEASE_LTO) {
        $env:CARGO_PROFILE_RELEASE_LTO = 'off'
        $env:CARGO_PROFILE_RELEASE_CODEGEN_UNITS = '16'
        $env:CARGO_PROFILE_RELEASE_OPT_LEVEL = '1'
        $env:CARGO_PROFILE_RELEASE_INCREMENTAL = 'true'
    }

    Push-Location $Lattice
    try {
        cargo build --release -p nai-fabric-host --target $Target
        if ($LASTEXITCODE -ne 0) {
            throw "bundle-host-for-product: cargo build -p nai-fabric-host failed (exit $LASTEXITCODE)"
        }
    }
    finally {
        Pop-Location
    }
}
finally {
    foreach ($key in $profileKeys) {
        [Environment]::SetEnvironmentVariable($key, $savedProfile[$key], 'Process')
    }
}

# ── resolve and verify the built artifact ───────────────────────────────────
$targetRoot = if ($env:CARGO_TARGET_DIR) {
    if ([System.IO.Path]::IsPathRooted($env:CARGO_TARGET_DIR)) {
        $env:CARGO_TARGET_DIR
    } else {
        Join-Path $Lattice $env:CARGO_TARGET_DIR
    }
} else {
    Join-Path $Lattice 'target'
}
$src = Join-Path $targetRoot "$Target\release\nai-fabric-host.exe"
if (-not (Test-Path -LiteralPath $src)) {
    throw "bundle-host-for-product: built host missing at $src"
}

# ── stage the package payload ───────────────────────────────────────────────
New-Item -ItemType Directory -Path $OutDir -Force | Out-Null
$dst = Join-Path $OutDir 'nai-fabric-host.exe'
Copy-Item -LiteralPath $src -Destination $dst -Force -ErrorAction Stop
$sha = (Get-FileHash -LiteralPath $dst -Algorithm SHA256).Hash.ToLowerInvariant()
$sizeMb = [math]::Round((Get-Item -LiteralPath $dst).Length / 1MB, 1)

# ── signer identity (the manifest carries it; consumers verify before exec) ──
$sig = Get-AuthenticodeSignature -LiteralPath $dst
if ($RequireSignature -and $sig.Status -ne 'Valid') {
    throw "bundle-host-for-product: $dst is not Authenticode-Valid ($($sig.Status)). Sign it before packaging."
}

$manifest = [ordered]@{
    version          = $version
    protocol_min     = $ProtocolMin
    protocol_max     = $ProtocolMax
    target           = $Target
    sha256           = $sha
    built_at_unix_ms = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()
    source           = 'NAI-Lattice'
    git              = $gitDesc
    signature_status = $sig.Status.ToString()
}
if ($sig.SignerCertificate) {
    $manifest['signer_subject'] = $sig.SignerCertificate.Subject
    $manifest['signer_thumbprint'] = $sig.SignerCertificate.Thumbprint
}
$manifestJson = $manifest | ConvertTo-Json -Depth 4
$manifestPath = Join-Path $OutDir 'host-manifest.json'
$manifestTmp = Join-Path $OutDir 'host-manifest.json.tmp'
[System.IO.File]::WriteAllText($manifestTmp, $manifestJson, [System.Text.UTF8Encoding]::new($false))
[System.IO.File]::Move($manifestTmp, $manifestPath, $true)

Write-Host ("[fabric-host] staged {0} v{1} ({2} MB) -> {3}" -f 'nai-fabric-host.exe', $version, $sizeMb, $dst)
Write-Host ("[fabric-host] manifest -> {0}  sha256={1}" -f $manifestPath, $sha.Substring(0, 12))
