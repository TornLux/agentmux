# Start the agentmux broker.
#
# Default mode: detached background process (Start-Process). Use
# -Foreground to invoke broker.exe directly, blocking the current
# shell — useful for debugging since logs and panics appear inline.
#
# Singleton via %LOCALAPPDATA%\agentmux\broker.pid: a live PID owned by
# a process named `broker` blocks the launch; a dead/foreign PID is
# treated as stale and cleared.
#
# IMPORTANT: claude inherits its working directory from the broker, not
# from claude-attach. Whichever directory you launch this script in
# (or whatever you pass via -WorkingDirectory) becomes claude's cwd —
# which decides claude's "trust this directory?" prompt and the project
# the model sees. Stop the broker first to retarget.

[CmdletBinding()]
param(
    [string]$WorkingDirectory = (Get-Location).Path,
    [switch]$Foreground
)

$ErrorActionPreference = "Stop"

$root = Split-Path -Parent $PSScriptRoot
# Release zips lay binaries in bin/; cargo builds in target/release/.
$candidates = @(
    (Join-Path $root "bin\broker.exe"),
    (Join-Path $root "target\release\broker.exe")
)
$exe = $candidates | Where-Object { Test-Path $_ } | Select-Object -First 1
if (-not $exe) {
    throw "broker.exe not found in $($candidates -join ' or '). Build with: cargo build --release"
}

if (-not (Test-Path $WorkingDirectory)) {
    throw "WorkingDirectory does not exist: $WorkingDirectory"
}
$WorkingDirectory = (Resolve-Path -LiteralPath $WorkingDirectory).Path

$dataDir = Join-Path $env:LOCALAPPDATA "agentmux"
New-Item -ItemType Directory -Path $dataDir -Force | Out-Null
$pidFile = Join-Path $dataDir "broker.pid"

# Singleton check. Three states for an existing pid:
#   1. dead pid          → stale file, clear and proceed
#   2. live + responsive → broker is up, refuse
#   3. live + unresponsive on /sessions → broker is mid-shutdown
#      (e.g. user just clicked tray "Stop broker"); the http handler
#      replied "ok" but the 200ms graceful-exit window hasn't elapsed.
#      Wait up to 5s for it to exit, then proceed.
if (Test-Path -LiteralPath $pidFile) {
    $existingPid = (Get-Content -LiteralPath $pidFile -Raw).Trim()
    if ($existingPid) {
        $proc = Get-Process -Id $existingPid -ErrorAction SilentlyContinue
        if ($proc -and $proc.ProcessName -eq 'broker') {
            # Double-probe with a 300ms gap. axum's graceful exit
            # writes the /shutdown response BEFORE actually closing
            # the listener, so a single probe right after a tray
            # "Stop broker" click can land in that ~50-300ms window
            # where the broker still answers /sessions but is
            # tearing down. Two consecutive successful probes mean
            # the broker is genuinely serving.
            $responsive = $false
            try {
                $null = Invoke-RestMethod "http://127.0.0.1:8765/sessions" `
                    -TimeoutSec 1 -ErrorAction Stop
                Start-Sleep -Milliseconds 300
                $null = Invoke-RestMethod "http://127.0.0.1:8765/sessions" `
                    -TimeoutSec 1 -ErrorAction Stop
                $responsive = $true
            } catch {}

            if ($responsive) {
                Write-Warning "broker is already running (pid $existingPid)."
                Write-Warning "Stop it first via .\scripts\stop-broker.ps1 if you want to relaunch."
                return
            }

            Write-Host "broker (pid $existingPid) is mid-shutdown — waiting up to 5s for exit..."
            $deadline = (Get-Date).AddSeconds(5)
            while (-not $proc.HasExited -and (Get-Date) -lt $deadline) {
                Start-Sleep -Milliseconds 100
                $proc.Refresh()
            }
            if (-not $proc.HasExited) {
                Write-Warning "broker (pid $existingPid) did not exit within 5s; aborting."
                Write-Warning "Force-stop with: .\scripts\stop-broker.ps1"
                return
            }
            Write-Host "broker exited; clearing pid file and continuing"
            Remove-Item -LiteralPath $pidFile -Force -ErrorAction SilentlyContinue
        } else {
            Write-Host "stale pid file (pid $existingPid no longer a broker) — clearing"
            Remove-Item -LiteralPath $pidFile -Force
        }
    }
}

if (-not $env:RUST_LOG) { $env:RUST_LOG = "info" }

if ($Foreground) {
    Write-Host "broker running in foreground (Ctrl+C to stop)"
    Write-Host "  exe:    $exe"
    Write-Host "  cwd:    $WorkingDirectory"
    Write-Host "  logs:   $(Join-Path $dataDir 'logs')"
    Write-Host ""
    Push-Location $WorkingDirectory
    try {
        & $exe --cwd $WorkingDirectory
    } finally {
        Pop-Location
    }
    return
}

# Background mode. tracing-appender writes daily-rolling files into
# %LOCALAPPDATA%\agentmux\logs\broker.YYYY-MM-DD.log directly; we keep
# stderr captured so early-startup eprintln! / panics still surface.
$err = Join-Path $dataDir "broker.stderr.log"

$p = Start-Process -FilePath $exe `
    -ArgumentList @("--cwd", $WorkingDirectory) `
    -WorkingDirectory $WorkingDirectory `
    -RedirectStandardError $err `
    -WindowStyle Hidden `
    -PassThru

Write-Host "broker started: pid $($p.Id)"
Write-Host "  cwd:    $WorkingDirectory"
Write-Host "  pid:    $pidFile"
Write-Host "  logs:   $(Join-Path $dataDir 'logs')"
Write-Host "  stderr: $err"
