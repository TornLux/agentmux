# Launch platform-discord.exe as a detached background process.
#
# Reads %LOCALAPPDATA%\agentmux\discord.toml for everything except the
# bot token, which must be in the env var named by `token_env`
# (default DISCORD_BOT_TOKEN). The bot exits early if that var is unset
# — start-discord then surfaces the error from the stderr log.

[CmdletBinding()]
param(
    [string]$ConfigPath
)

$ErrorActionPreference = "Stop"

$root = Split-Path -Parent $PSScriptRoot
$exe  = Join-Path $root "target\release\platform-discord.exe"

if (-not (Test-Path $exe)) {
    throw "platform-discord.exe not found at $exe. Run: cargo build --release"
}

$dataDir = Join-Path $env:LOCALAPPDATA "agentmux"
New-Item -ItemType Directory -Path $dataDir -Force | Out-Null

$cfgPath = if ($ConfigPath) { $ConfigPath } else { Join-Path $dataDir "discord.toml" }
if (-not (Test-Path $cfgPath)) {
    throw "discord.toml not found at $cfgPath. Run: .\init-discord-config.ps1"
}

$out = Join-Path $dataDir "platform-discord.stdout.log"
$err = Join-Path $dataDir "platform-discord.stderr.log"

if (-not $env:RUST_LOG) { $env:RUST_LOG = "info,serenity=warn" }

$envOverrides = @{}
if ($ConfigPath) {
    $envOverrides["AGENT_DISCORD_CONFIG"] = $ConfigPath
}

# Pass AGENT_DISCORD_CONFIG by setting it on this PS scope before spawn —
# Start-Process inherits the parent's env. We restore the old value after.
$priorAGENT = $env:AGENT_DISCORD_CONFIG
foreach ($k in $envOverrides.Keys) {
    Set-Item -Path "Env:$k" -Value $envOverrides[$k]
}
try {
    $p = Start-Process -FilePath $exe `
        -WorkingDirectory $root `
        -RedirectStandardOutput $out `
        -RedirectStandardError $err `
        -WindowStyle Hidden `
        -PassThru
} finally {
    $env:AGENT_DISCORD_CONFIG = $priorAGENT
}

Write-Host "platform-discord started: pid $($p.Id)"
Write-Host "  config: $cfgPath"
Write-Host "  stdout: $out"
Write-Host "  stderr: $err"
Write-Host ""
Write-Host "tip: tail the log with"
Write-Host "  Get-Content -Wait $err"
