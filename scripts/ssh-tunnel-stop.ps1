# Stop an SSH tunnel previously started by ssh-tunnel-start.ps1.
#
# Quick usage
# -----------
#   .\scripts\ssh-tunnel-stop.ps1 -Side broker     # stop the A-side tunnel
#   .\scripts\ssh-tunnel-stop.ps1 -Side viewer     # stop the B-side tunnel
#   .\scripts\ssh-tunnel-stop.ps1 -Side all        # stop both halves
#                                                  # (useful on a host that
#                                                  # ran both roles during
#                                                  # testing)
#
# How it works
# ------------
# Reads the PID at %LOCALAPPDATA%\agentmux\ssh-tunnel-<side>.pid, verifies
# the PID still belongs to ssh.exe or plink.exe (guard against the OS
# recycling the PID into something else), Stop-Process, and removes the
# PID file. If the file is missing, empty, or the PID is dead/foreign,
# the script reports it and cleans up rather than failing.
#
# Safe to re-run; missing/stale state is reported, not fatal.

[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [ValidateSet('broker','viewer','all')]
    [string]$Side
)

$ErrorActionPreference = 'Stop'

$pidDir = Join-Path $env:LOCALAPPDATA 'agentmux'
$sides  = if ($Side -eq 'all') { @('broker','viewer') } else { @($Side) }

foreach ($s in $sides) {
    $pidFile = Join-Path $pidDir "ssh-tunnel-$s.pid"
    if (-not (Test-Path $pidFile)) {
        Write-Host "No $s tunnel PID file at $pidFile (already stopped?)." -ForegroundColor DarkGray
        continue
    }

    $tunnelPid = Get-Content $pidFile -ErrorAction SilentlyContinue
    if (-not $tunnelPid) {
        Write-Host "PID file $pidFile is empty; removing." -ForegroundColor DarkGray
        Remove-Item $pidFile -Force
        continue
    }

    $proc = Get-Process -Id $tunnelPid -ErrorAction SilentlyContinue
    if (-not $proc) {
        Write-Host "$s tunnel PID $tunnelPid not running; cleaning PID file." -ForegroundColor DarkGray
        Remove-Item $pidFile -Force
        continue
    }

    # Recycled-PID guard: refuse to kill anything that isn't ours.
    if ($proc.ProcessName -notmatch '^(ssh|plink)$') {
        Write-Warning "PID $tunnelPid is $($proc.ProcessName), not ssh/plink. Refusing to kill. Delete $pidFile manually if it is stale."
        continue
    }

    Stop-Process -Id $tunnelPid -Force
    Remove-Item $pidFile -Force
    Write-Host "Stopped $s tunnel (PID $tunnelPid)." -ForegroundColor Green
}
