# agentmux unified entrypoint.
#
# Wraps the individual scripts under scripts/ and the agentmux-cli helper
# binary into a single CLI. Run `.\agentmux help` for the verb list.

[CmdletBinding()]
param(
    [Parameter(Position = 0)]
    [string]$Command = "help",

    [Parameter(ValueFromRemainingArguments = $true)]
    [string[]]$Rest
)

$ErrorActionPreference = "Stop"

# --- paths --------------------------------------------------------------

$root        = $PSScriptRoot
$scriptsDir  = Join-Path $root "scripts"

# Release zips ship `bin/`; cargo builds drop into `target/release/`.
# Prefer bin/ when present so a release-extracted tree always works.
$binCandidates = @(
    (Join-Path $root "bin"),
    (Join-Path $root "target\release")
)
$bin = $binCandidates | Where-Object { Test-Path (Join-Path $_ "broker.exe") } | Select-Object -First 1
if (-not $bin) { $bin = $binCandidates[1] }   # fall back so error messages point somewhere sensible

$agentmuxCli = Join-Path $bin "agentmux-cli.exe"
$brokerExe   = Join-Path $bin "broker.exe"
$attachExe   = Join-Path $bin "claude-attach.exe"
$discordExe  = Join-Path $bin "platform-discord.exe"

$dataDir   = Join-Path $env:LOCALAPPDATA "agentmux"
$brokerCfg = Join-Path $dataDir "config.toml"
$discordCfg = Join-Path $dataDir "discord.toml"
$hooksCfg  = Join-Path $env:USERPROFILE ".claude\settings.json"
$pidFile   = Join-Path $dataDir "broker.pid"

# --- helpers ------------------------------------------------------------

function Require-Binary([string]$path, [string]$label) {
    if (-not (Test-Path $path)) {
        Write-Host "✗ $label not found at $path" -ForegroundColor Red
        Write-Host "  Run: cargo build --release   (or download a release zip)" -ForegroundColor Yellow
        exit 1
    }
}

function Get-BrokerPid {
    if (-not (Test-Path $pidFile)) { return $null }
    $pidText = (Get-Content $pidFile -Raw -ErrorAction SilentlyContinue).Trim()
    if (-not $pidText) { return $null }
    $proc = Get-Process -Id $pidText -ErrorAction SilentlyContinue
    if ($proc -and $proc.ProcessName -eq 'broker') { return [int]$pidText }
    return $null
}

function Resolve-ConfigPath([string]$which) {
    switch ($which) {
        "broker"  { return $brokerCfg }
        "discord" { return $discordCfg }
        "hooks"   { return $hooksCfg  }
        default   { throw "unknown config kind: $which (use broker|discord|hooks)" }
    }
}

function Pick-Editor {
    if ($env:VISUAL) { return $env:VISUAL }
    if ($env:EDITOR) { return $env:EDITOR }
    if (Get-Command code -ErrorAction SilentlyContinue) { return "code" }
    return "notepad"
}

function Read-Token {
    $secure = Read-Host "  Discord bot token (input hidden)" -AsSecureString
    $bstr = [Runtime.InteropServices.Marshal]::SecureStringToBSTR($secure)
    try {
        return [Runtime.InteropServices.Marshal]::PtrToStringBSTR($bstr)
    } finally {
        [Runtime.InteropServices.Marshal]::ZeroFreeBSTR($bstr)
    }
}

function Verify-DiscordToken([string]$token) {
    try {
        $headers = @{ Authorization = "Bot $token" }
        $me = Invoke-RestMethod "https://discord.com/api/v10/users/@me" -Headers $headers -TimeoutSec 5
        return @{ ok = $true; user = $me.username; id = $me.id }
    } catch {
        return @{ ok = $false; err = $_.Exception.Message }
    }
}

# --- commands -----------------------------------------------------------

function Cmd-Help {
    @"
agentmux — Claude Code multi-session daemon (Windows)

Usage: .\agentmux <command> [args]

Setup
  init                  Run the first-time setup wizard
  hooks install         Wire Claude Code hooks (also runs in `init`)
  hooks uninstall       Remove hooks from ~\.claude\settings.json
  hooks check           Validate hooks configuration

Daily ops
  start [--no-discord]  Start broker (and Discord bot if configured)
  start --foreground    Start broker in the current shell (Ctrl+C to stop)
  stop                  Stop broker and Discord bot
  status                Show what's running and active sessions
  attach [name]         Open a terminal viewer (wraps claude-attach.exe)
  logs [broker|discord|events]
                        Tail a log stream

Configuration
  config edit [broker|discord|hooks]      Open in `$env:EDITOR` / VS Code / notepad
  config dir                              Open the config folder in Explorer
  config path [broker|discord|hooks]      Print the absolute path
  config show [broker|discord]            Print the file contents
  config check                            Validate all configs
  config set <broker|discord> <key> <val> Set a scalar field
  config unset <broker|discord> <key>     Remove a field

Discord bridge
  discord setup                  Walk through token + channel + user setup
  discord token                  Re-prompt + verify the bot token
  discord users add <id>         Append to allowed_user_ids
  discord users remove <id>      Remove from allowed_user_ids
  discord channels add <id>      Append to channel_ids
  discord channels remove <id>   Remove from channel_ids

  help                  Print this message
"@ | Write-Host
}

function Cmd-Init {
    Write-Host ""
    Write-Host "agentmux setup wizard" -ForegroundColor Cyan
    Write-Host "─────────────────────"
    Write-Host ""

    # 1 — prereqs
    Write-Host "[1/5] Checking prerequisites..." -ForegroundColor Cyan
    $missing = $false
    foreach ($pair in @(
        @{ Path = $brokerExe;  Label = "broker.exe" },
        @{ Path = $attachExe;  Label = "claude-attach.exe" },
        @{ Path = $agentmuxCli; Label = "agentmux-cli.exe" }
    )) {
        if (Test-Path $pair.Path) {
            Write-Host "  ✓ $($pair.Label)"
        } else {
            Write-Host "  ✗ $($pair.Label) missing at $($pair.Path)" -ForegroundColor Red
            $missing = $true
        }
    }
    if ($missing) {
        Write-Host ""
        Write-Host "Build first with:  cargo build --release" -ForegroundColor Yellow
        Write-Host "(or download a pre-built release zip)" -ForegroundColor Yellow
        return
    }

    $claudeCmd = Get-Command claude -ErrorAction SilentlyContinue
    if ($claudeCmd) {
        Write-Host "  ✓ claude CLI on PATH ($($claudeCmd.Source))"
    } else {
        Write-Host "  ⚠ claude CLI not on PATH — broker will fail to spawn sessions" -ForegroundColor Yellow
        Write-Host "    Install: npm install -g @anthropic-ai/claude-code" -ForegroundColor Yellow
    }
    Write-Host ""

    # 2 — hooks
    Write-Host "[2/5] Claude Code hooks" -ForegroundColor Cyan
    Write-Host "  Hooks let agentmux receive 'turn complete' events. Without them,"
    Write-Host "  Discord won't get replies and auto-resume can't detect 'ready'."
    $ans = Read-Host "  Install hooks now? [Y/n]"
    if ($ans -ne "n" -and $ans -ne "N") {
        & (Join-Path $scriptsDir "install-hooks.ps1")
    }
    Write-Host ""

    # 3 — broker config
    Write-Host "[3/5] Broker configuration" -ForegroundColor Cyan
    if (Test-Path $brokerCfg) {
        Write-Host "  ✓ broker config already exists at $brokerCfg"
    } else {
        $ans = Read-Host "  Write a default broker config.toml? [Y/n]"
        if ($ans -ne "n" -and $ans -ne "N") {
            & (Join-Path $scriptsDir "init-config.ps1")
        }
    }
    Write-Host ""

    # 4 — discord (optional)
    Write-Host "[4/5] Discord IM bridge (optional)" -ForegroundColor Cyan
    $ans = Read-Host "  Set up Discord? [y/N]"
    if ($ans -eq "y" -or $ans -eq "Y") {
        Cmd-DiscordSetup
    } else {
        Write-Host "  skipped — re-run later with .\agentmux discord setup"
    }
    Write-Host ""

    # 5 — start
    Write-Host "[5/5] Start services" -ForegroundColor Cyan
    $ans = Read-Host "  Start broker now? [Y/n]"
    if ($ans -ne "n" -and $ans -ne "N") {
        Cmd-Start @()
    }
    Write-Host ""
    Write-Host "Setup complete." -ForegroundColor Green
    Write-Host "  .\agentmux attach        # enter the TUI"
    Write-Host "  .\agentmux status        # check what's running"
    Write-Host "  .\agentmux help          # full command list"
}

function Cmd-Start([string[]]$Argv) {
    Require-Binary $brokerExe "broker.exe"

    $foreground = ($Argv -contains "--foreground")
    if ($foreground) {
        # Foreground mode blocks until Ctrl+C; Discord bot would never
        # get launched after broker returns. If you want both, run them
        # in separate shells.
        & (Join-Path $scriptsDir "start-broker.ps1") -Foreground
        return
    }

    & (Join-Path $scriptsDir "start-broker.ps1")

    if ($Argv -contains "--no-discord") { return }
    if (-not (Test-Path $discordCfg)) { return }

    $token = [Environment]::GetEnvironmentVariable("DISCORD_BOT_TOKEN", "User")
    if (-not $token) {
        Write-Host "discord.toml present but DISCORD_BOT_TOKEN env var is empty — bot not started" -ForegroundColor Yellow
        Write-Host "  set with: .\agentmux discord token" -ForegroundColor Yellow
        return
    }
    $env:DISCORD_BOT_TOKEN = $token
    & (Join-Path $scriptsDir "start-discord.ps1")
}

function Cmd-Stop {
    Get-Process -Name platform-discord -ErrorAction SilentlyContinue | ForEach-Object {
        Stop-Process -Id $_.Id -Force
        Write-Host "stopped platform-discord (pid $($_.Id))"
    }
    & (Join-Path $scriptsDir "stop-broker.ps1")
}

function Cmd-Status {
    $bp = Get-BrokerPid
    if ($bp) {
        Write-Host "broker:  running (pid $bp)" -ForegroundColor Green
        try {
            $sessions = Invoke-RestMethod "http://127.0.0.1:8765/sessions" -TimeoutSec 2
            Write-Host "  sessions: $($sessions.Count)"
            foreach ($s in $sessions) {
                $marker = " "
                Write-Host ("  $marker {0,-18} {1,-12} viewers={2}  cwd={3}" -f $s.name, $s.state, $s.viewers, $s.cwd)
            }
        } catch {
            Write-Host "  (failed to query /sessions — broker still booting?)" -ForegroundColor Yellow
        }
    } else {
        Write-Host "broker:  not running" -ForegroundColor Gray
    }

    $disc = Get-Process -Name platform-discord -ErrorAction SilentlyContinue
    if ($disc) {
        Write-Host "discord: running (pid $($disc.Id))" -ForegroundColor Green
    } elseif (Test-Path $discordCfg) {
        Write-Host "discord: configured but not running" -ForegroundColor Gray
    } else {
        Write-Host "discord: not configured" -ForegroundColor Gray
    }
}

function Cmd-Attach([string[]]$Argv) {
    Require-Binary $attachExe "claude-attach.exe"
    if ($Argv -and $Argv.Count -gt 0) {
        # Treat first positional as session name unless it already starts with --
        $first = $Argv[0]
        if ($first -and -not $first.StartsWith("--")) {
            & $attachExe --session $first @($Argv[1..($Argv.Count - 1)])
            return
        }
    }
    & $attachExe @args
}

function Cmd-Logs([string[]]$Argv) {
    $which = if ($Argv -and $Argv.Count -gt 0) { $Argv[0] } else { "broker" }
    switch ($which) {
        "broker" {
            $logsDir = Join-Path $dataDir "logs"
            $latest = Get-ChildItem $logsDir -Filter "broker.*.log" -ErrorAction SilentlyContinue |
                Sort-Object LastWriteTime -Descending | Select-Object -First 1
            if ($latest) { Get-Content $latest.FullName -Wait -Tail 30 }
            else { Write-Host "no broker logs in $logsDir" }
        }
        "discord" {
            $f = Join-Path $dataDir "platform-discord.stdout.log"
            if (Test-Path $f) { Get-Content $f -Wait -Tail 30 }
            else { Write-Host "no discord log at $f" }
        }
        "events" {
            $f = Join-Path $dataDir "events.jsonl"
            if (Test-Path $f) { Get-Content $f -Wait -Tail 20 }
            else { Write-Host "no events log at $f" }
        }
        default {
            Write-Host "usage: .\agentmux logs [broker|discord|events]"
        }
    }
}

# --- config subcommands -------------------------------------------------

function Cmd-Config([string[]]$Argv) {
    if (-not $Argv -or $Argv.Count -eq 0) { $Argv = @("show") }
    $sub = $Argv[0]
    $rest = if ($Argv.Count -gt 1) { $Argv[1..($Argv.Count - 1)] } else { @() }
    switch ($sub) {
        "edit"   { Cmd-ConfigEdit  $rest }
        "dir"    { Cmd-ConfigDir }
        "path"   { Cmd-ConfigPath  $rest }
        "show"   { Cmd-ConfigShow  $rest }
        "check"  { Cmd-ConfigCheck $rest }
        "set"    { Cmd-ConfigSet   $rest }
        "unset"  { Cmd-ConfigUnset $rest }
        default  { Write-Host "Usage: .\agentmux config <edit|dir|path|show|check|set|unset> [...]" }
    }
}

function Cmd-ConfigDir {
    if (-not (Test-Path $dataDir)) {
        New-Item -ItemType Directory -Path $dataDir | Out-Null
    }
    Start-Process explorer.exe $dataDir
    Write-Host "opened $dataDir"
}

function Cmd-ConfigPath([string[]]$Argv) {
    $which = if ($Argv -and $Argv.Count -gt 0) { $Argv[0] } else { "broker" }
    Write-Output (Resolve-ConfigPath $which)
}

function Cmd-ConfigShow([string[]]$Argv) {
    $which = if ($Argv -and $Argv.Count -gt 0) { $Argv[0] } else { "broker" }
    $path = Resolve-ConfigPath $which
    if (Test-Path $path) {
        Get-Content $path
    } else {
        Write-Host "$path does not exist"
    }
}

function Cmd-ConfigEdit([string[]]$Argv) {
    $which = if ($Argv -and $Argv.Count -gt 0) { $Argv[0] } else { "broker" }
    $path  = Resolve-ConfigPath $which

    if (-not (Test-Path $path)) {
        Write-Host "$path does not exist"
        $ans = Read-Host "Create from template? [Y/n]"
        if ($ans -ne "n" -and $ans -ne "N") {
            switch ($which) {
                "broker"  { & (Join-Path $scriptsDir "init-config.ps1") }
                "discord" { & (Join-Path $scriptsDir "init-discord-config.ps1") }
                default   { Write-Host "no template available for $which"; return }
            }
        }
        if (-not (Test-Path $path)) { return }
    }

    $editor = Pick-Editor
    Write-Host "opening $path with $editor ..."
    & $editor $path
    if ($editor -eq "notepad") {
        # notepad blocks until closed; safe to validate now.
        Write-Host ""
        Cmd-ConfigCheck @($which)
    } else {
        Write-Host ""
        Write-Host "(editor returned. After saving, run: .\agentmux config check)" -ForegroundColor Yellow
    }
}

function Cmd-ConfigCheck([string[]]$Argv) {
    Require-Binary $agentmuxCli "agentmux-cli.exe"
    $kinds = if ($Argv -and $Argv.Count -gt 0) { $Argv } else { @("broker", "discord", "hooks") }
    foreach ($kind in $kinds) {
        $path = Resolve-ConfigPath $kind
        Write-Host ""
        Write-Host "── $kind ($path)" -ForegroundColor Cyan
        & $agentmuxCli config check $path --kind $kind
    }
    Write-Host ""
}

function Cmd-ConfigSet([string[]]$Argv) {
    Require-Binary $agentmuxCli "agentmux-cli.exe"
    if (-not $Argv -or $Argv.Count -lt 3) {
        Write-Host "usage: .\agentmux config set <broker|discord> <key> <value>"
        return
    }
    $path = Resolve-ConfigPath $Argv[0]
    if (-not (Test-Path $path)) {
        Write-Host "$path does not exist — run: .\agentmux config edit $($Argv[0])" -ForegroundColor Yellow
        return
    }
    & $agentmuxCli config set $path $Argv[1] $Argv[2]
}

function Cmd-ConfigUnset([string[]]$Argv) {
    Require-Binary $agentmuxCli "agentmux-cli.exe"
    if (-not $Argv -or $Argv.Count -lt 2) {
        Write-Host "usage: .\agentmux config unset <broker|discord> <key>"
        return
    }
    $path = Resolve-ConfigPath $Argv[0]
    if (-not (Test-Path $path)) {
        Write-Host "$path does not exist" -ForegroundColor Yellow
        return
    }
    & $agentmuxCli config unset $path $Argv[1]
}

# --- discord subcommands ------------------------------------------------

function Cmd-Discord([string[]]$Argv) {
    if (-not $Argv -or $Argv.Count -eq 0) {
        Write-Host "Usage: .\agentmux discord <setup|token|users|channels|start>"
        return
    }
    $sub = $Argv[0]
    $rest = if ($Argv.Count -gt 1) { $Argv[1..($Argv.Count - 1)] } else { @() }
    switch ($sub) {
        "setup"    { Cmd-DiscordSetup }
        "token"    { Cmd-DiscordToken }
        "users"    { Cmd-DiscordList "allowed_user_ids" $rest }
        "channels" { Cmd-DiscordList "channel_ids"      $rest }
        "start"    { & (Join-Path $scriptsDir "start-discord.ps1") }
        default    { Write-Host "unknown discord subcommand: $sub" }
    }
}

function Cmd-DiscordSetup {
    Require-Binary $agentmuxCli "agentmux-cli.exe"
    Write-Host "Discord IM setup" -ForegroundColor Cyan
    Write-Host "  Token lives in env var (default DISCORD_BOT_TOKEN), never on disk."
    Write-Host ""

    # Ensure config exists
    if (-not (Test-Path $discordCfg)) {
        Write-Host "  writing discord.toml template..."
        & (Join-Path $scriptsDir "init-discord-config.ps1")
    }

    # Token
    $existing = [Environment]::GetEnvironmentVariable("DISCORD_BOT_TOKEN", "User")
    if ($existing) {
        Write-Host "  DISCORD_BOT_TOKEN already set ($($existing.Length) chars)"
        $ans = Read-Host "  Replace token? [y/N]"
        if ($ans -eq "y" -or $ans -eq "Y") { Cmd-DiscordToken }
    } else {
        Cmd-DiscordToken
    }

    # Channel + user IDs (loop, accept many; blank line stops)
    Write-Host ""
    Write-Host "  Channel IDs (Discord > Settings > Advanced > Developer Mode → right-click channel → Copy ID)"
    Write-Host "  Enter one per line; blank to finish."
    while ($true) {
        $cid = Read-Host "    channel"
        if (-not $cid) { break }
        & $agentmuxCli config array-add $discordCfg "channel_ids" $cid
    }

    Write-Host ""
    Write-Host "  Allowed user IDs (right-click your avatar → Copy ID)"
    Write-Host "  Enter one per line; blank to finish."
    while ($true) {
        $uid = Read-Host "    user"
        if (-not $uid) { break }
        & $agentmuxCli config array-add $discordCfg "allowed_user_ids" $uid
    }

    Write-Host ""
    Cmd-ConfigCheck @("discord")
    Write-Host "Discord configured." -ForegroundColor Green
    Write-Host "Re-open this PowerShell window so the env var propagates, then run .\agentmux start."
}

function Cmd-DiscordToken {
    Write-Host ""
    $token = Read-Token
    if (-not $token -or $token.Length -lt 30) {
        Write-Host "  invalid token (too short)" -ForegroundColor Red
        return
    }
    Write-Host "  verifying with Discord API..."
    $r = Verify-DiscordToken $token
    if (-not $r.ok) {
        Write-Host "  ✗ token rejected: $($r.err)" -ForegroundColor Red
        return
    }
    Write-Host "  ✓ token valid (bot user: $($r.user) id=$($r.id))" -ForegroundColor Green
    [Environment]::SetEnvironmentVariable("DISCORD_BOT_TOKEN", $token, "User")
    Write-Host "  saved to User-scope env var DISCORD_BOT_TOKEN" -ForegroundColor Green
    Write-Host "  re-open PowerShell sessions for new value to take effect."
}

function Cmd-DiscordList([string]$key, [string[]]$Argv) {
    Require-Binary $agentmuxCli "agentmux-cli.exe"
    if (-not $Argv -or $Argv.Count -lt 2) {
        Write-Host "usage: .\agentmux discord users|channels add|remove <id>"
        return
    }
    $action = $Argv[0]
    $value = $Argv[1]
    if (-not (Test-Path $discordCfg)) {
        Write-Host "discord.toml does not exist — run: .\agentmux discord setup" -ForegroundColor Yellow
        return
    }
    switch ($action) {
        "add"    { & $agentmuxCli config array-add    $discordCfg $key $value }
        "remove" { & $agentmuxCli config array-remove $discordCfg $key $value }
        default  { Write-Host "unknown action: $action (use add|remove)" }
    }
}

# --- hooks subcommands --------------------------------------------------

function Cmd-Hooks([string[]]$Argv) {
    $action = if ($Argv -and $Argv.Count -gt 0) { $Argv[0] } else { "install" }
    switch ($action) {
        "install"   { & (Join-Path $scriptsDir "install-hooks.ps1") }
        "uninstall" { & (Join-Path $scriptsDir "install-hooks.ps1") -Uninstall }
        "check"     { Cmd-ConfigCheck @("hooks") }
        default     { Write-Host "usage: .\agentmux hooks <install|uninstall|check>" }
    }
}

# --- dispatch -----------------------------------------------------------

switch ($Command) {
    ""        { Cmd-Help }
    "help"    { Cmd-Help }
    "--help"  { Cmd-Help }
    "-h"      { Cmd-Help }

    "init"    { Cmd-Init }
    "start"   { Cmd-Start  $Rest }
    "stop"    { Cmd-Stop }
    "status"  { Cmd-Status }
    "attach"  { Cmd-Attach $Rest }
    "logs"    { Cmd-Logs   $Rest }

    "config"  { Cmd-Config  $Rest }
    "discord" { Cmd-Discord $Rest }
    "hooks"   { Cmd-Hooks   $Rest }

    default {
        Write-Host "unknown command: $Command" -ForegroundColor Red
        Write-Host ""
        Cmd-Help
        exit 1
    }
}
