# Start the agentmux broker as a detached background process.
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
    [string]$WorkingDirectory = (Get-Location).Path
)

$ErrorActionPreference = "Stop"

$root = Split-Path -Parent $PSScriptRoot
$exe  = Join-Path $root "target\release\broker.exe"

if (-not (Test-Path $exe)) {
    throw "broker.exe not found at $exe. Run: cargo build --release"
}

if (-not (Test-Path $WorkingDirectory)) {
    throw "WorkingDirectory does not exist: $WorkingDirectory"
}
$WorkingDirectory = (Resolve-Path -LiteralPath $WorkingDirectory).Path

$dataDir = Join-Path $env:LOCALAPPDATA "agentmux"
New-Item -ItemType Directory -Path $dataDir -Force | Out-Null
$pidFile = Join-Path $dataDir "broker.pid"

# Singleton check.
if (Test-Path -LiteralPath $pidFile) {
    $existingPid = (Get-Content -LiteralPath $pidFile -Raw).Trim()
    if ($existingPid) {
        $proc = Get-Process -Id $existingPid -ErrorAction SilentlyContinue
        if ($proc -and $proc.ProcessName -eq 'broker') {
            Write-Warning "broker is already running (pid $existingPid)."
            Write-Warning "Stop it first via .\scripts\stop-broker.ps1 if you want to relaunch."
            return
        }
        Write-Host "stale pid file (pid $existingPid no longer a broker) — clearing"
        Remove-Item -LiteralPath $pidFile -Force
    }
}

# tracing-appender writes to %LOCALAPPDATA%\agentmux\logs\broker.YYYY-MM-DD.log
# directly. Keep stderr redirected for early-startup eprintln! / panics.
$err = Join-Path $dataDir "broker.stderr.log"

if (-not $env:RUST_LOG) { $env:RUST_LOG = "info" }

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
