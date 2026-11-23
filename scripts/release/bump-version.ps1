# Copyright (c) NAI / NANOTRIK.AI
# Licensed under the MIT OR Apache-2.0 license.
#
# bump-version.ps1 - bump the Rust workspace and the two npm packages in one
# auditable step. Cargo requires a literal version on every path dependency, so
# those literals are updated together with the workspace version.
#
# Usage:
#   pwsh ./scripts/release/bump-version.ps1 `
#       -CargoVersion 0.1.1 `
#       -NpmClientVersion 0.2.1 `
#       -NpmMcpVersion 0.1.1

[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$CargoVersion,

    [Parameter(Mandatory = $true)]
    [string]$NpmClientVersion,

    [Parameter(Mandatory = $true)]
    [string]$NpmMcpVersion
)

$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
Set-Location -LiteralPath $repo

function Assert-Version([string]$Version, [string]$Name) {
    if ($Version -notmatch '^\d+\.\d+\.\d+([-.+][0-9A-Za-z.-]+)?$') {
        throw "Invalid $Name version: $Version"
    }
}

Assert-Version $CargoVersion 'Cargo'
Assert-Version $NpmClientVersion 'npm client'
Assert-Version $NpmMcpVersion 'npm mcp'

# Workspace version (single source for every crate using version.workspace).
$rootPath = Join-Path $repo 'Cargo.toml'
$root = Get-Content -LiteralPath $rootPath -Raw
$updated = [regex]::Replace($root, '(?m)^version = "[^"]+"$', "version = `"$CargoVersion`"", 1)
if ($updated -eq $root) { throw 'Workspace version line was not found.' }
Set-Content -LiteralPath $rootPath -Value $updated -NoNewline

# Cargo path dependencies require literal versions; keep them in lockstep.
$manifests = Get-ChildItem -LiteralPath $repo -Recurse -File -Filter Cargo.toml |
    Where-Object { $_.FullName -notmatch '\\target\\' }
$changed = 0
foreach ($manifest in $manifests) {
    $text = Get-Content -LiteralPath $manifest.FullName -Raw
    $new = [regex]::Replace(
        $text,
        '(path\s*=\s*"[^"]+"\s*,\s*version\s*=\s*")[^"]+(")',
        '${1}' + $CargoVersion + '${2}'
    )
    if ($new -ne $text) {
        Set-Content -LiteralPath $manifest.FullName -Value $new -NoNewline
        $changed++
    }
}
Write-Host "Cargo workspace -> $CargoVersion ($changed manifest(s) updated)"

# npm versions and the client dependency range.
& npm version $NpmClientVersion --workspace packages/naf-client-ts --no-git-tag-version --allow-same-version | Out-Host
if ($LASTEXITCODE -ne 0) { throw 'npm version failed for packages/naf-client-ts' }
& npm version $NpmMcpVersion --workspace packages/mcp-nai-lattice --no-git-tag-version --allow-same-version | Out-Host
if ($LASTEXITCODE -ne 0) { throw 'npm version failed for packages/mcp-nai-lattice' }
& npm pkg set "dependencies.@cyberluke/nai-lattice-client=^$NpmClientVersion" --workspace packages/mcp-nai-lattice | Out-Host
if ($LASTEXITCODE -ne 0) { throw 'npm pkg set failed for the MCP client dependency' }

Write-Host "npm client -> $NpmClientVersion; mcp -> $NpmMcpVersion"
Write-Host 'Run cargo check --workspace and commit the manifests + Cargo.lock.'
