# Register the daily Lattice maintenance task (per-user).
#
# The fabric host is per-user, so the task runs as the current user and only
# while that user is logged on (Interactive logon). It executes the bounded,
# scheduled lifecycle operation:
#
#     nai-lattice-cli maintenance prune --scheduled --auto --yes --stop
#
# `--scheduled` is a successful no-op (exit 0) when `auto_prune` is false in
# the maintenance policy. `--auto` additionally gates the run on the shared
# storage pressure (the same measurement as `maintenance status`,
# `runtime.storage.status` and the dashboard summary): normal or unknown
# pressure exits 0 without planning, leasing or touching the host; only
# elevated/critical pressure runs the safe prune. `--stop` cooperatively stops
# the host, prunes logs/audit/backups (and superseded runtimes only if the
# policy opts in), then restarts it. It never touches the canonical ledger or
# CAS, and never removes the active runtime or the live maintenance lease.
#
# Usage:
#   pwsh -File .\scripts\register-maintenance-task.ps1
#   pwsh -File .\scripts\register-maintenance-task.ps1 -Time 04:30
#   pwsh -File .\scripts\register-maintenance-task.ps1 -CliPath C:\tools\nai-lattice-cli.exe
#   pwsh -File .\scripts\register-maintenance-task.ps1 -Unregister

[CmdletBinding(SupportsShouldProcess)]
param(
    [string]$CliPath = '',
    [string]$TaskName = 'NAI Lattice Maintenance',
    [string]$Time = '03:00',
    [switch]$Unregister
)

$ErrorActionPreference = 'Stop'

function Resolve-LatticeCli {
    if ($CliPath) {
        if (-not (Test-Path -LiteralPath $CliPath)) { throw "CLI not found: $CliPath" }
        return (Resolve-Path -LiteralPath $CliPath).Path
    }
    $candidates = @()
    if ($env:NAI_LATTICE_CLI) { $candidates += $env:NAI_LATTICE_CLI }
    $repoRoot = Split-Path -Parent $PSScriptRoot
    $candidates += @(
        (Join-Path $repoRoot 'target\x86_64-pc-windows-msvc\release\nai-lattice-cli.exe'),
        (Join-Path $repoRoot 'target\release\nai-lattice-cli.exe'),
        (Join-Path $repoRoot 'target\debug\nai-lattice-cli.exe'),
        (Join-Path $env:LOCALAPPDATA 'NAI\Lattice\tools\nai-lattice-cli.exe')
    )
    foreach ($candidate in $candidates) {
        if ($candidate -and (Test-Path -LiteralPath $candidate)) { return (Resolve-Path -LiteralPath $candidate).Path }
    }
    $resolved = Get-Command 'nai-lattice-cli' -ErrorAction SilentlyContinue
    if ($resolved) { return $resolved.Source }
    throw ("nai-lattice-cli.exe not found. Build it with " +
        "`cargo build --release -p nai-lattice-cli`, install it, set NAI_LATTICE_CLI, " +
        "or pass -CliPath.")
}

if ($Unregister) {
    $existing = Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
    if (-not $existing) {
        Write-Host "[lattice-maintenance] task '$TaskName' is not registered"
        return
    }
    if ($PSCmdlet.ShouldProcess($TaskName, 'Unregister scheduled task')) {
        Unregister-ScheduledTask -TaskName $TaskName -Confirm:$false
        Write-Host "[lattice-maintenance] unregistered '$TaskName'"
    }
    return
}

if ($Time -notmatch '^\d{2}:\d{2}$') { throw "-Time must be HH:mm (got '$Time')" }
$cli = Resolve-LatticeCli
$workdir = Split-Path -Parent $cli

$action = New-ScheduledTaskAction -Execute $cli `
    -Argument 'maintenance prune --scheduled --auto --yes --stop' `
    -WorkingDirectory $workdir
$trigger = New-ScheduledTaskTrigger -Daily -At $Time
$settings = New-ScheduledTaskSettingsSet `
    -StartWhenAvailable `
    -MultipleInstances IgnoreNew `
    -ExecutionTimeLimit (New-TimeSpan -Hours 1) `
    -AllowStartIfOnBatteries `
    -DontStopIfGoingOnBatteries
$principal = New-ScheduledTaskPrincipal `
    -UserId ("{0}\{1}" -f $env:USERDOMAIN, $env:USERNAME) `
    -LogonType Interactive `
    -RunLevel Limited

if ($PSCmdlet.ShouldProcess($TaskName, "Register daily maintenance at $Time")) {
    $existing = Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
    if ($existing) {
        Unregister-ScheduledTask -TaskName $TaskName -Confirm:$false
    }
    Register-ScheduledTask `
        -TaskName $TaskName `
        -Action $action `
        -Trigger $trigger `
        -Settings $settings `
        -Principal $principal `
        -Description 'NAI Lattice pressure-gated lifecycle maintenance (prune; host stop/restart; audit).' | Out-Null
    Write-Host "[lattice-maintenance] registered '$TaskName' daily at $Time"
    Write-Host "[lattice-maintenance] CLI: $cli"
    Write-Host "[lattice-maintenance] run now: Start-ScheduledTask -TaskName '$TaskName'"
}
