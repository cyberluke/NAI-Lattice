# Copyright (c) NAI / NANOTRIK.AI
# Licensed under the MIT OR Apache-2.0 license.
#
# load-env.ps1 - load a local .env file into the process environment.
#
# Never prints values. Supports:
#   KEY=value
#   export KEY=value
#   KEY="quoted value"
#   # comments
#
# Usage:
#   . ./scripts/release/load-env.ps1
#   . ./scripts/release/load-env.ps1 -Path <secure-dir>\release.env

[CmdletBinding()]
param(
    [string]$Path = (Join-Path (Split-Path -Parent (Split-Path -Parent $PSScriptRoot)) '.env.local')
)

$ErrorActionPreference = 'Stop'

if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
    throw "Env file not found: $Path"
}

Get-Content -LiteralPath $Path | ForEach-Object {
    $line = $_.Trim()
    if (-not $line -or $line.StartsWith('#')) { return }
    $line = $line -replace '^(?:export\s+)', ''
    $eq = $line.IndexOf('=')
    if ($eq -lt 1) { return }
    $name = $line.Substring(0, $eq).Trim()
    if ($name -notmatch '^[A-Za-z_][A-Za-z0-9_]*$') { return }
    $value = $line.Substring($eq + 1).Trim()
    if ($value.Length -ge 2) {
        $first = $value[0]
        $last = $value[$value.Length - 1]
        if (($first -eq '"' -and $last -eq '"') -or ($first -eq "'" -and $last -eq "'")) {
            $value = $value.Substring(1, $value.Length - 2)
        }
    }
    Set-Item -Path "Env:$name" -Value $value
}
