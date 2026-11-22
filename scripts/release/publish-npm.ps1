# Copyright (c) NAI / NANOTRIK.AI
# Licensed under the MIT OR Apache-2.0 license.
#
# publish-npm.ps1 - publish the TypeScript packages to npm in dependency order.
# Local step only: no CI/CD, no OIDC provenance.
#
# Prerequisites:
#   npm login                  # once per machine; token is not in the repo
#   npm run build:ts           # prepublishOnly also builds, but build first
#
# Usage:
#   pwsh ./scripts/release/publish-npm.ps1              # dry run
#   pwsh ./scripts/release/publish-npm.ps1 -Execute     # real publish

[CmdletBinding()]
param(
    [switch]$Execute
)

$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
Set-Location -LiteralPath $repo

if (-not $Execute) {
    Write-Host 'would publish @cyberluke/nai-lattice-client'
    Write-Host 'would publish @cyberluke/mcp-nai-lattice'
    Write-Host ''
    Write-Host 'Dry run only. Re-run with -Execute to publish.'
    return
}

$who = npm whoami 2>$null
if ($LASTEXITCODE -ne 0 -or -not $who) {
    throw 'npm is not authenticated. Run: npm login'
}

npm publish -w '@cyberluke/nai-lattice-client'
if ($LASTEXITCODE -ne 0) { throw 'npm publish failed for @cyberluke/nai-lattice-client' }

npm publish -w '@cyberluke/mcp-nai-lattice'
if ($LASTEXITCODE -ne 0) { throw 'npm publish failed for @cyberluke/mcp-nai-lattice' }

Write-Host 'Published both npm packages.'
