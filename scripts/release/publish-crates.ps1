# Copyright (c) NAI / NANOTRIK.AI
# Licensed under the MIT OR Apache-2.0 license.
#
# publish-crates.ps1 - publish the NAI Lattice crates to crates.io in dependency
# order. Local step only: no CI/CD.
#
# Prerequisites:
#   cargo login <token>        # once per machine; token is not in the repo
#   clean git working tree     # crates.io records the VCS state
#
# It retries a failed publish, because the crates.io index takes a short moment
# to expose a freshly published dependency to the next package.
#
# Usage:
#   pwsh ./scripts/release/publish-crates.ps1              # dry run (prints order)
#   pwsh ./scripts/release/publish-crates.ps1 -Execute     # real publish

[CmdletBinding()]
param(
    [switch]$Execute,

    [switch]$AllowDirty,

    [int]$RetrySeconds = 30,

    [int]$MaxRetries = 20
)

$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
Set-Location -LiteralPath $repo

if (-not $AllowDirty) {
    $dirty = git status --porcelain
    if ($dirty) {
        throw "Working tree is dirty. Commit the release first, or pass -AllowDirty."
    }
}

# Cargo requires a literal version in a `path` dependency. Refuse to publish a
# graph where those literals drifted from the workspace version.
$meta = (cargo metadata --no-deps --format-version 1 2>$null) | ConvertFrom-Json
$versions = @($meta.packages | Where-Object { $_.name -like 'nai-lattice-*' } |
    Select-Object -ExpandProperty version -Unique)
if ($versions.Count -ne 1) {
    throw "Crate versions differ: $($versions -join ', ')"
}
$expected = [string]$versions[0]
$mismatch = @()
Get-ChildItem -LiteralPath $repo -Recurse -File -Filter Cargo.toml |
    Where-Object { $_.FullName -notmatch '\\target\\' } |
    ForEach-Object {
        Select-String -Path $_.FullName -Pattern 'path\s*=\s*"[^"]+",\s*version\s*=\s*"([^"]+)"' |
            ForEach-Object {
                if ($_.Matches[0].Groups[1].Value -ne $expected) {
                    $mismatch += "$($_.Path):$($_.LineNumber)"
                }
            }
    }
if ($mismatch.Count -gt 0) {
    throw "Dependency version mismatch (expected $expected):`n$($mismatch -join "`n")"
}

# Topological order derived from the workspace manifests.
$order = @(
    'nai-lattice-core',
    'nai-lattice-git',
    'nai-lattice-hotstore',
    'nai-lattice-supervisor',
    'nai-lattice-protocol',
    'nai-lattice-cas',
    'nai-lattice-vector',
    'nai-lattice-search',
    'nai-lattice-graph',
    'nai-lattice-hotstore-fjall',
    'nai-lattice-hotstore-surrealkv',
    'nai-lattice-client',
    'nai-lattice-embeddings',
    'nai-lattice-ledger',
    'nai-lattice-aaf',
    'nai-lattice-observability',
    'nai-lattice-provider-appcontent',
    'nai-lattice-provider-filesystem',
    'nai-lattice-provider-opfs',
    'nai-lattice-provider-windows-search',
    'nai-lattice-testkit',
    'nai-lattice-lakehouse',
    'nai-lattice-query',
    'nai-lattice-gastown',
    'nai-lattice-runtime'
)

foreach ($pkg in $order) {
    # Idempotent: a retry after a partial publish skips versions already live.
    try {
        $existing = Invoke-RestMethod -Uri "https://crates.io/api/v1/crates/$pkg" `
            -Headers @{ 'User-Agent' = 'nai-lattice-release (contact: cyberluke)' } `
            -TimeoutSec 20 -ErrorAction Stop
        $live = @($existing.versions | ForEach-Object { $_.num })
    }
    catch {
        $live = @()
    }
    if ($live -contains $expected) {
        Write-Host "==> $pkg $expected already published; skipping"
        continue
    }

    if (-not $Execute) {
        Write-Host "would publish $pkg"
        continue
    }

    $published = $false
    for ($attempt = 1; $attempt -le $MaxRetries; $attempt++) {
        Write-Host "==> cargo publish -p $pkg (attempt $attempt/$MaxRetries)"
        $cargoArgs = @('publish', '-p', $pkg)
        if ($AllowDirty) { $cargoArgs += '--allow-dirty' }
        $output = & cargo @cargoArgs 2>&1
        $output | ForEach-Object { Write-Host $_ }
        if ($LASTEXITCODE -eq 0) {
            $published = $true
            break
        }
        if (($output | Out-String) -match 'verified email address is required') {
            throw 'crates.io requires a verified email address. Verify it at https://crates.io/settings/profile, then rerun this script.'
        }
        if ($attempt -lt $MaxRetries) {
            Write-Warning "publish failed; waiting ${RetrySeconds}s for the index, then retrying."
            Start-Sleep -Seconds $RetrySeconds
        }
    }
    if (-not $published) {
        throw "cargo publish failed for $pkg after $MaxRetries attempts."
    }
}

if (-not $Execute) {
    Write-Host ""
    Write-Host "Dry run only. Re-run with -Execute to publish."
}
