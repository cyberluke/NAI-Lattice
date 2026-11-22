# Copyright (c) NAI / NANOTRIK.AI
# Licensed under the MIT OR Apache-2.0 license.
#
# r2-release.ps1 - publish NAI Lattice artifacts to the sovereign Cloudflare R2
# bucket served at <release-host>.
#
# This is a LOCAL release step. There is no CI/CD, no GitHub Actions and no
# GitHub Release: the operator runs this script on the release box.
#
# Layout (deliberately namespaced so it never collides with NAI OS):
#   s3://<bucket>/nai-lattice/<version>/<asset>
#   s3://<bucket>/nai-lattice/<version>/<asset>.sha256
#   s3://<bucket>/nai-lattice/latest.json
#
# Usage:
#   $env:CLOUDFLARE_R2_BUCKET   = '<bucket>'
#   $env:CLOUDFLARE_R2_ENDPOINT = 'https://<accountid>.r2.cloudflarestorage.com'
#   pwsh ./scripts/release/r2-release.ps1 `
#       -Version 0.1.0 `
#       -Artifacts target/release/nai-fabric-host.exe,target/release/nai-lattice-cli.exe,dist/nai-lattice/host-manifest.json `
#       -Latest
#
# Credentials: AWS_ACCESS_KEY_ID / AWS_SECRET_ACCESS_KEY must be an R2 token
# scoped to this bucket (ObjectRead/ObjectWrite).

[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$Version,

    [Parameter(Mandatory = $true)]
    [string[]]$Artifacts,

    [string]$Bucket = $(if ($env:CLOUDFLARE_R2_BUCKET) { $env:CLOUDFLARE_R2_BUCKET } elseif ($env:R2_BUCKET) { $env:R2_BUCKET } else { '' }),

    [string]$Endpoint = $(if ($env:CLOUDFLARE_R2_ENDPOINT) { $env:CLOUDFLARE_R2_ENDPOINT } elseif ($env:R2_SERVER) { $env:R2_SERVER } else { '' }),

    [string]$PublicBase = $(if ($env:R2_PUBLIC_URL) { $env:R2_PUBLIC_URL } else { '<release-host>' }),

    [string]$Prefix = $(if ($env:R2_FOLDER_PREFIX) { $env:R2_FOLDER_PREFIX } else { 'nai-lattice' }),

    [string]$Product = 'nai-lattice',

    [string]$Git = '',

    [string]$Channel = 'latest',

    [switch]$Latest,

    [switch]$SkipUpload
)

$ErrorActionPreference = 'Stop'

if (-not $env:AWS_ACCESS_KEY_ID -and $env:R2_ACCESS_KEY) {
    $env:AWS_ACCESS_KEY_ID = $env:R2_ACCESS_KEY
}
if (-not $env:AWS_SECRET_ACCESS_KEY -and $env:R2_SECRET_KEY) {
    $env:AWS_SECRET_ACCESS_KEY = $env:R2_SECRET_KEY
}
if (-not $env:AWS_DEFAULT_REGION) { $env:AWS_DEFAULT_REGION = 'auto' }

if (-not $Bucket -or -not $Endpoint) {
    throw 'Set CLOUDFLARE_R2_BUCKET / CLOUDFLARE_R2_ENDPOINT or R2_BUCKET / R2_SERVER before running.'
}
if (-not (Get-Command aws -ErrorAction SilentlyContinue)) {
    throw 'AWS CLI (aws) is required for R2 uploads.'
}
if ($Endpoint -notmatch '^https?://') { $Endpoint = "https://$Endpoint" }
$Endpoint = $Endpoint.TrimEnd('/')

$version = $Version.Trim() -replace '^v', ''
if ($version -notmatch '^[0-9A-Za-z][0-9A-Za-z._-]*$') {
    throw "Unsafe version label: $Version"
}

$prefixName = $Prefix.Trim().Trim('/')
if (-not $prefixName -or $prefixName -notmatch '^[0-9A-Za-z][0-9A-Za-z._/-]*$') {
    throw "Unsafe R2 prefix: $Prefix"
}
$prefix = "$prefixName/$version"
$publicBase = $PublicBase.TrimEnd('/')
# R2_PUBLIC_URL may already include the product prefix
# (e.g. <release-host>/NAI-Lattice). Strip it so the public URI is
# always <root>/<prefix>/<version>/<asset>, never a doubled prefix.
$publicSuffix = "/$prefixName"
if ($publicBase.EndsWith($publicSuffix, [StringComparison]::OrdinalIgnoreCase)) {
    $publicBase = $publicBase.Substring(0, $publicBase.Length - $publicSuffix.Length)
}

function Get-Sha256([string]$Path) {
    (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant()
}

$assets = @()
foreach ($artifact in $Artifacts) {
    $path = [IO.Path]::GetFullPath($artifact)
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
        throw "Artifact not found: $path"
    }
    $assets += [pscustomobject]@{
        Path = $path
        Name = [IO.Path]::GetFileName($path)
        Key  = "$prefix/$([IO.Path]::GetFileName($path))"
    }
}

# Per-asset SHA-256 sidecars, in sha256sum format.
$sumsDir = Join-Path $env:TEMP "nai-lattice-r2-$(Get-Random)"
New-Item -ItemType Directory -Path $sumsDir -Force | Out-Null

try {
    $sidecars = @()
    foreach ($asset in $assets) {
        $hash = Get-Sha256 $asset.Path
        $sidecar = Join-Path $sumsDir "$($asset.Name).sha256"
        "$hash  $($asset.Name)" | Set-Content -LiteralPath $sidecar -Encoding ascii
        $sidecars += [pscustomobject]@{
            Path = $sidecar
            Key  = "$($asset.Key).sha256"
        }
    }

    $latestBody = [ordered]@{
        schemaVersion = 1
        product       = $Product
        channel       = $Channel
        version       = $version
        publishedAt   = [DateTime]::UtcNow.ToString('o')
        git           = $(if ($Git) { $Git } else { (& git -C (Split-Path -Parent (Split-Path -Parent $PSScriptRoot)) rev-parse --short HEAD 2>$null) })
        artifacts     = [ordered]@{}
    }
    foreach ($asset in $assets) {
        $latestBody.artifacts[$asset.Name] = [ordered]@{
            uri    = "$publicBase/$($asset.Key)"
            sha256 = (Get-Sha256 $asset.Path)
            size   = (Get-Item -LiteralPath $asset.Path).Length
        }
    }
    $latestPath = Join-Path $sumsDir 'latest.json'
    ($latestBody | ConvertTo-Json -Depth 8) | Set-Content -LiteralPath $latestPath -Encoding utf8

    Write-Host "==> Publishing NAI Lattice v$version to $publicBase"
    foreach ($asset in $assets) {
        Write-Host "  - $($asset.Key) ($([math]::Round((Get-Item $asset.Path).Length / 1MB, 1)) MB)"
    }

    if ($SkipUpload) {
        Write-Host '==> [dry-run] would upload:'
        foreach ($asset in $assets + $sidecars) {
            Write-Host "    aws s3 cp `"$($asset.Path)`" s3://$Bucket/$($asset.Key) --endpoint-url $Endpoint"
        }
        if ($Latest) {
            Write-Host "    aws s3 cp `"$latestPath`" s3://$Bucket/$prefixName/latest.json --endpoint-url $Endpoint --cache-control no-cache"
        }
        return
    }

    foreach ($asset in $assets + $sidecars) {
        & aws s3 cp $asset.Path "s3://$Bucket/$($asset.Key)" `
            --endpoint-url $Endpoint --quiet `
            --cache-control 'public, max-age=31536000, immutable'
        if ($LASTEXITCODE -ne 0) {
            throw "R2 upload failed for $($asset.Key)"
        }
    }

    if ($Latest) {
        & aws s3 cp $latestPath "s3://$Bucket/$prefixName/latest.json" `
            --endpoint-url $Endpoint --quiet --cache-control 'no-cache'
        if ($LASTEXITCODE -ne 0) {
            throw "$prefixName/latest.json upload failed"
        }
        Write-Host "==> $prefixName/latest.json -> v$version ($Channel)"
    }

    Write-Host ''
    Write-Host 'Release available at:'
    foreach ($asset in $assets) {
        Write-Host "  $($asset.Name.PadRight(28)) $publicBase/$($asset.Key)"
    }
    if ($Latest) {
        Write-Host "  latest.json                 $publicBase/$prefixName/latest.json"
    }
}
finally {
    Remove-Item -LiteralPath $sumsDir -Recurse -Force -ErrorAction SilentlyContinue
}
