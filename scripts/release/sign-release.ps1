# Copyright (c) NAI / NANOTRIK.AI
# Licensed under the MIT OR Apache-2.0 license.
#
# sign-release.ps1 - Authenticode-sign NAI Lattice artifacts with the sovereign
# NAI code-signing leaf, then verify the result.
#
# Signing MUST happen before packaging: the MSIX/bundle and host-manifest record
# the bytes that were signed, so re-signing afterward invalidates them.
#
# Usage:
#   pwsh ./scripts/release/sign-release.ps1 `
#       -Files target/x86_64-pc-windows-msvc/release/nai-fabric-host.exe,
#              target/x86_64-pc-windows-msvc/release/nai-lattice-cli.exe
#
# Password: -Password, then the file at -PasswordFile, then $env:NAI_CA_PASSWORD,
# then a prompt. Never log the password.

[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string[]]$Files,

    [string]$Pfx = '.cert/NAILattice.pfx',

    [string]$PasswordFile = '.cert/NAILattice.pfx.pwd',

    [string]$Password = '',

    [string]$TimestampUrl = 'http://timestamp.digicert.com',

    [switch]$SkipTimestamp
)

$ErrorActionPreference = 'Stop'

$pfxPath = [IO.Path]::GetFullPath($Pfx)
if (-not (Test-Path -LiteralPath $pfxPath -PathType Leaf)) {
    throw "Signing PFX not found: $pfxPath"
}

if ($Password) {
    $pfxPassword = $Password
}
elseif (Test-Path -LiteralPath $PasswordFile -PathType Leaf) {
    $pfxPassword = (Get-Content -LiteralPath $PasswordFile -Raw).TrimEnd()
}
elseif ($env:NAI_CA_PASSWORD) {
    $pfxPassword = $env:NAI_CA_PASSWORD
}
else {
    $secure = Read-Host -Prompt "Password for $pfxPath" -AsSecureString
    $pfxPassword = [Runtime.InteropServices.Marshal]::PtrToStringBSTR(
        [Runtime.InteropServices.Marshal]::SecureStringToBSTR($secure)
    )
}

$sdkRoot = 'C:\Program Files (x86)\Windows Kits\10\bin'
$sdkVersion = Get-ChildItem -LiteralPath $sdkRoot -Directory -ErrorAction SilentlyContinue |
    Where-Object { $_.Name -match '^\d+\.\d+\.\d+\.\d+$' } |
    Sort-Object { [version]$_.Name } -Descending |
    Select-Object -First 1
if (-not $sdkVersion) {
    throw "Windows SDK not found under $sdkRoot"
}
$signtool = Join-Path $sdkVersion.FullName 'x64\signtool.exe'
if (-not (Test-Path -LiteralPath $signtool)) {
    throw "signtool.exe not found: $signtool"
}

foreach ($file in $Files) {
    $target = [IO.Path]::GetFullPath($file)
    if (-not (Test-Path -LiteralPath $target -PathType Leaf)) {
        throw "Artifact not found: $target"
    }

    # Never put the password in a log line; it is passed as an argv element only.
    $signArgs = @('sign', '/fd', 'SHA256', '/f', $pfxPath)
    if ($pfxPassword.Length -gt 0) { $signArgs += @('/p', $pfxPassword) }
    if (-not $SkipTimestamp -and $TimestampUrl) {
        $signArgs += @('/tr', $TimestampUrl, '/td', 'SHA256')
    }
    $signArgs += $target

    Write-Host "==> Signing $([IO.Path]::GetFileName($target))"
    & $signtool @signArgs
    if ($LASTEXITCODE -ne 0) {
        throw "signtool sign failed for $target (exit $LASTEXITCODE)"
    }

    & $signtool verify /pa /all /v $target | Out-Null
    if ($LASTEXITCODE -ne 0) {
        throw "signtool verify failed for $target (exit $LASTEXITCODE)"
    }

    $sig = Get-AuthenticodeSignature -LiteralPath $target
    if ($sig.Status -ne 'Valid') {
        throw "Authenticode verification failed for $target`: $($sig.StatusMessage)"
    }
    Write-Host "    OK: $($sig.Status) - $($sig.SignerCertificate.Subject)"
    Write-Host "        thumbprint $($sig.SignerCertificate.Thumbprint)"
}

Write-Host ""
Write-Host "Signed and verified $($Files.Count) artifact(s)."
