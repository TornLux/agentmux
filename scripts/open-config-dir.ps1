# Open %LOCALAPPDATA%\agentmux\ in File Explorer.
# Creates the directory if it doesn't exist yet (so the action always
# succeeds even on a fresh install).

$ErrorActionPreference = "Stop"

$dir = Join-Path $env:LOCALAPPDATA "agentmux"
New-Item -ItemType Directory -Path $dir -Force | Out-Null
Start-Process explorer.exe $dir
Write-Host "opened: $dir"
