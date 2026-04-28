# Write a documented template config.toml at
# %LOCALAPPDATA%\agentmux\config.toml. Every field is commented out so
# defaults apply unmodified — uncomment a line to override.
#
# Usage:
#   .\init-config.ps1            # create if missing, refuse if exists
#   .\init-config.ps1 -Force     # overwrite (existing file is backed up to .bak)
#   .\init-config.ps1 -Edit      # also open the file in notepad afterwards

[CmdletBinding()]
param(
    [switch]$Force,
    [switch]$Edit
)

$ErrorActionPreference = "Stop"

$dir = Join-Path $env:LOCALAPPDATA "agentmux"
$cfg = Join-Path $dir "config.toml"

New-Item -ItemType Directory -Path $dir -Force | Out-Null

if ((Test-Path -LiteralPath $cfg) -and -not $Force) {
    Write-Warning "config.toml already exists at: $cfg"
    Write-Warning "Re-run with -Force to overwrite (a .bak is made first)."
    if ($Edit) {
        Start-Process notepad.exe $cfg
    }
    return
}

if (Test-Path -LiteralPath $cfg) {
    $bak = "$cfg.bak"
    Copy-Item -LiteralPath $cfg -Destination $bak -Force
    Write-Host "backup: $bak"
}

$content = @'
# agentmux broker config — uncomment a line to override its default.
# Loaded by broker.exe and claude-attach.exe at startup. Empty file or
# any missing field is equivalent to "use the default".

# HTTP control plane host:port. broker binds; viewer + hooks resolve
# their broker URL from http://<this>.
# http_addr = "127.0.0.1:8765"

# Win32 named pipe path. Must match between broker and viewer.
# pipe_name = '\\.\pipe\claude-broker'

# argv used when broker starts a fresh session. The command after
# broker.exe on the CLI overrides this value at runtime.
# default_command = ["claude", "--dangerously-skip-permissions"]

# Per-session ring buffer cap (bytes). Bounds the replay sent to a
# newly-attached viewer.
# ring_cap_bytes = 524288

# Auto-hibernate Idle sessions whose user-side activity has been quiet
# for this many seconds. 0 disables the scanner.
# hibernate_idle_secs = 86400

# Override path for sessions.toml. Empty = default
# %LOCALAPPDATA%\agentmux\sessions.toml.
# sessions_toml_path = ""

# Override path for broker.pid. Empty = default
# %LOCALAPPDATA%\agentmux\broker.pid.
# pid_file_path = ""

# Override directory for daily-rolling broker logs (kept 7 days).
# Empty = default %LOCALAPPDATA%\agentmux\logs.
# log_dir = ""

# Default value of `auto_resume` on newly-created sessions when the
# create request doesn't pass one explicitly. false = sessions are
# ephemeral by default (forgotten on broker restart); true = always
# restored. Per-session `auto_resume` (set at create time or via
# the `/sessions/:k/persist` endpoint, e.g. Discord `!persist on`)
# always wins over this default.
# auto_resume_default = false

# Bearer token required for **non-loopback** HTTP / WebSocket
# requests. Loopback (127.0.0.1, ::1) is always exempt so existing
# local tooling continues to work without any token. Empty = the
# broker rejects every non-loopback connection (default; safe).
# Generate one with:  .\agentmux config token --set
# attach_token = ""
'@

$utf8NoBom = New-Object System.Text.UTF8Encoding($false)
[System.IO.File]::WriteAllText($cfg, $content, $utf8NoBom)
Write-Host "wrote: $cfg"

if ($Edit) {
    Start-Process notepad.exe $cfg
}
