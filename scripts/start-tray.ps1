# Launch agentmux-tray.exe as a detached background process.
#
# The tray reads %LOCALAPPDATA%\agentmux\config.toml (same as broker)
# for the broker URL — no separate config. Subscribes to broker /ws,
# polls /sessions every 5s, owns the system-tray icon and toast
# notifications. Exits clean if a tray instance is already running
# (single-instance handshake via named pipe).

[CmdletBinding()]
param()

$ErrorActionPreference = "Stop"

$root = Split-Path -Parent $PSScriptRoot
$candidates = @(
    (Join-Path $root "bin\agentmux-tray.exe"),
    (Join-Path $root "target\release\agentmux-tray.exe")
)
$exe = $candidates | Where-Object { Test-Path $_ } | Select-Object -First 1
if (-not $exe) {
    throw "agentmux-tray.exe not found in $($candidates -join ' or '). Build with: cargo build --release"
}

$dataDir = Join-Path $env:LOCALAPPDATA "agentmux"
New-Item -ItemType Directory -Path $dataDir -Force | Out-Null

$out = Join-Path $dataDir "agentmux-tray.stdout.log"
$err = Join-Path $dataDir "agentmux-tray.stderr.log"

if (-not $env:RUST_LOG) { $env:RUST_LOG = "info,agentmux_tray=info" }

$p = Start-Process -FilePath $exe `
    -WorkingDirectory $root `
    -RedirectStandardOutput $out `
    -RedirectStandardError $err `
    -WindowStyle Hidden `
    -PassThru

Write-Host "agentmux-tray started: pid $($p.Id)"
Write-Host "  stderr: $err"
Write-Host ""
Write-Host "tip: tail the log with"
Write-Host "  Get-Content -Wait $err"
