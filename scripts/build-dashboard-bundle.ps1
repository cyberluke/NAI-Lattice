# Build and seal the Lattice dashboard bundle that the fabric host embeds at
# compile time.
#
# The host embeds `packages/lattice-dashboard/dist` via `build.rs` (size +
# BLAKE3 verified against `bundle-manifest.json`). A binary built without this
# step still works, but serves the legacy dashboard only: `/lattice/next`
# redirects to `/lattice` and `dashboard_version` is null. The canonical
# packaging step (`packaging/bundle-host-for-product.ps1`) runs this first so a
# release never silently omits the shell.
#
# Usage:
#   pwsh -File .\scripts\build-dashboard-bundle.ps1
#   pwsh -File .\scripts\build-dashboard-bundle.ps1 -SkipInstall   # deps present

[CmdletBinding()]
param(
    [string]$Lattice = (Split-Path -Parent $PSScriptRoot),
    # Skip `npm ci` when node_modules is already installed (local re-runs).
    [switch]$SkipInstall
)

$ErrorActionPreference = 'Stop'

$package = Join-Path $Lattice 'packages\lattice-dashboard'
if (-not (Test-Path -LiteralPath (Join-Path $package 'package.json'))) {
    throw "build-dashboard-bundle: package not found at $package"
}

Push-Location $Lattice
try {
    if (-not $SkipInstall) {
        # ci installs exactly the lockfile (reproducible release input).
        npm ci --no-audit --no-fund
        if ($LASTEXITCODE -ne 0) {
            throw "build-dashboard-bundle: npm ci failed (exit $LASTEXITCODE)"
        }
    }

    npm -w lattice-dashboard run typecheck
    if ($LASTEXITCODE -ne 0) {
        throw "build-dashboard-bundle: typecheck failed (exit $LASTEXITCODE)"
    }

    npm -w lattice-dashboard run build
    if ($LASTEXITCODE -ne 0) {
        throw "build-dashboard-bundle: vite build failed (exit $LASTEXITCODE)"
    }

    npm -w lattice-dashboard run verify:manifest
    if ($LASTEXITCODE -ne 0) {
        throw "build-dashboard-bundle: manifest verification failed (exit $LASTEXITCODE)"
    }
}
finally {
    Pop-Location
}

$manifestPath = Join-Path $package 'dist\bundle-manifest.json'
if (-not (Test-Path -LiteralPath $manifestPath)) {
    throw "build-dashboard-bundle: manifest missing at $manifestPath"
}
$manifest = Get-Content -LiteralPath $manifestPath -Raw | ConvertFrom-Json
$index = @($manifest.files) | Where-Object { $_.path -eq 'index.html' } | Select-Object -First 1
if (-not $index -or -not $index.blake3) {
    throw "build-dashboard-bundle: manifest has no verified index.html entry"
}

Write-Host ("[dashboard] sealed v{0} · index.html {1} B · blake3 {2}" -f `
        $manifest.version, $index.bytes, $index.blake3)
