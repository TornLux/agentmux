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

# Force UTF-8 console I/O so glyphs (✓, ─, …) render correctly on systems
# whose OEM code page is not 65001 (e.g. CP936 on zh-CN Windows).
try {
    [Console]::OutputEncoding = [System.Text.Encoding]::UTF8
    [Console]::InputEncoding  = [System.Text.Encoding]::UTF8
    $OutputEncoding           = [System.Text.Encoding]::UTF8
} catch {}

# --- paths --------------------------------------------------------------

$root        = $PSScriptRoot
$scriptsDir  = Join-Path $root "scripts"

# Release zips ship `bin/`; cargo builds drop into `target/release/`.
# Prefer bin/ when present so a release-extracted tree always works.
# target/debug/ is a third-tier fallback for `cargo build` (no
# --release) — works fine for development, just slower hooks/startup.
$binCandidates = @(
    (Join-Path $root "bin"),
    (Join-Path $root "target\release"),
    (Join-Path $root "target\debug")
)
$bin = $binCandidates | Where-Object { Test-Path (Join-Path $_ "broker.exe") } | Select-Object -First 1
if (-not $bin) { $bin = $binCandidates[1] }   # fall back so error messages point somewhere sensible
if ($bin -and ($bin -match '\\target\\debug$')) {
    Write-Host "(using debug build at $bin — slower, intended for development; cargo build --release for prod)" -ForegroundColor Yellow
}

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

function Test-HooksInstalled {
    # Returns $true iff Claude Code's settings.json has all four
    # agentmux hook entries (Stop, Notification, PreToolUse,
    # PostToolUse) wired to their respective exes. Match is by basename
    # (case-insensitive) so it stays robust to slash-shape and path-
    # move changes; install-hooks.ps1 handles re-canonicalisation if
    # asked.
    if (-not (Test-Path $hooksCfg)) { return $false }
    try {
        $raw = Get-Content -LiteralPath $hooksCfg -Raw -ErrorAction Stop
        if (-not $raw -or $raw.Trim().Length -eq 0) { return $false }
        $json = $raw | ConvertFrom-Json
        if (-not $json.hooks) { return $false }
        $found = @{ Stop = $false; Notification = $false; PreToolUse = $false; PostToolUse = $false }
        $needles = @{
            Stop         = "hook-stop.exe"
            Notification = "hook-notification.exe"
            PreToolUse   = "hook-pretool.exe"
            PostToolUse  = "hook-posttool.exe"
        }
        foreach ($evt in @("Stop", "Notification", "PreToolUse", "PostToolUse")) {
            $groups = $json.hooks.$evt
            if (-not $groups) { continue }
            foreach ($g in @($groups)) {
                $entries = $g.hooks
                if (-not $entries) { continue }
                foreach ($h in @($entries)) {
                    if ($h.type -ne "command" -or -not $h.command) { continue }
                    if ($h.command.ToLowerInvariant().Contains($needles[$evt])) {
                        $found[$evt] = $true
                    }
                }
            }
        }
        return ($found.Stop -and $found.Notification -and $found.PreToolUse -and $found.PostToolUse)
    } catch {
        return $false
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

function Invoke-BrokerJson {
    # Thin wrapper around Invoke-RestMethod for broker calls. Always
    # talks to loopback (broker's auth middleware exempts loopback so
    # no token plumbing here). Captures non-2xx response bodies so
    # callers can render the broker's structured error JSON instead of
    # PowerShell's default "Response status code does not indicate
    # success" message.
    param(
        [Parameter(Mandatory)] [string]$Method,
        [Parameter(Mandatory)] [string]$Path,
        $Body = $null,
        [int]$TimeoutSec = 30
    )
    $url = "http://127.0.0.1:8765$Path"
    $params = @{
        Method = $Method
        Uri = $url
        TimeoutSec = $TimeoutSec
        ErrorAction = 'Stop'
    }
    if ($null -ne $Body) {
        $params.Body = ($Body | ConvertTo-Json -Compress -Depth 10)
        $params.ContentType = 'application/json; charset=utf-8'
    }
    try {
        $resp = Invoke-RestMethod @params
        return @{ ok = $true; data = $resp }
    } catch {
        $status = 0
        $body = ""
        $resp = $_.Exception.Response
        if ($resp) {
            try { $status = [int]$resp.StatusCode } catch {}
            try {
                $stream = $resp.GetResponseStream()
                $reader = New-Object System.IO.StreamReader($stream)
                $body = $reader.ReadToEnd()
                $reader.Close()
            } catch {}
        }
        return @{ ok = $false; status = $status; body = $body; err = $_.Exception.Message }
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
  hooks install         Wire Claude Code Stop / Notification / PreToolUse / PostToolUse hooks
  hooks uninstall       Remove hooks from ~\.claude\settings.json
  hooks check           Validate hooks configuration

Daily ops
  start [--no-tray] [--no-discord]
                        Start broker + tray + Discord bot (if configured)
  start --foreground    Start broker in the current shell (Ctrl+C to stop)
  stop                  Stop broker, tray, and Discord bot
  restart               Stop + start (reloads config.toml / discord.toml)
  status                Show what's running and active sessions
  attach [name]         Open a local terminal viewer (named pipe)
                        --broker http://host:port --token <t>: connect over LAN
                        Or: open http://<broker>:8765/ in any browser
                        Or: right-click the tray icon → Attach <session>
  new <name> [-Cwd <path>] [-Persist | -Ephemeral]
                        Create a new broker session.
                        Default cwd = config.default_cwd, else broker's launch cwd.
  kill <name> [-Force]  Delete a session record (asks for confirmation
                        unless -Force).
  adopt --resume <id> [name] [--cwd <path>]
                        Bring an external claude conversation under broker.
                        Run AFTER exiting the local 'claude'.
  adopt <name>          Re-adopt a previously-demoted session.
  demote <name>         Hand a broker session back to local terminal control.
                        Broker kills claude and prints the resume command.
  logs [broker|discord|tray|events]
                        Tail a log stream

Configuration
  config edit [broker|discord|hooks]      Open in `$env:EDITOR` / VS Code / notepad
  config dir                              Open the config folder in Explorer
  config path [broker|discord|hooks]      Print the absolute path
  config show [broker|discord]            Print the file contents
  config check                            Validate all configs
  config set <broker|discord> <key> <val> Set a scalar field
  config unset <broker|discord> <key>     Remove a field
  config token [--set]                    Generate a 32-byte attach token
                                          (with --set, writes to broker.toml)
  config gui                              Open the GUI config editor (eframe)

Orchestrator
  orchestrator                   (Re-)configure boss/worker workflow:
                                  main_session, worker_thread_parent,
                                  dashboard_channel_id. Same step that runs
                                  in step 5 of agentmux init.

Discord bridge
  discord setup                  Walk through token + channel + user setup
  discord token                  Re-prompt + verify the bot token
  discord users add <id>         Append to allowed_user_ids
  discord users remove <id>      Remove from allowed_user_ids
  discord channels add <id>      Append to channel_ids
  discord channels remove <id>   Remove from channel_ids

  help                  Print this message
  help <verb>           Print help for a specific verb (also: <verb> --help)
"@ | Write-Host
}

# Per-verb help blocks. Called when a verb is invoked with `--help`,
# `-h`, no args (where that doesn't make sense), or via `agentmux help
# <verb>`.
function Show-VerbHelp([string]$verb) {
    switch ($verb) {
        "init" {
            @"
Usage: .\agentmux init

  Interactive first-time wizard. Six steps:
    1. prerequisite check (binaries, claude on PATH)
    2. install Claude Code hooks (Stop + Notification + PreToolUse + PostToolUse)
       — idempotent; entries are dedup'd by basename, so reinstalling from
         a different folder converges to a single canonical entry
    3. write broker config template (skipped if exists) + default_cwd prompt
    4. optional Discord setup (token + channels + users)
       — re-detects an already-configured Discord and skips
    5. optional orchestrator workflow (main_session + worker_thread_parent +
       dashboard_channel_id) — wires the boss/worker pattern from
       docs/orchestrator-prompt.md across config.toml + discord.toml
    6. start broker (and tray + Discord) in the background

  Re-runnable; already-done steps are skipped or reconfirmed.
"@
        }
        "start" {
            @"
Usage: .\agentmux start [--no-tray] [--no-discord | --foreground]

  Default: starts broker as a detached background process, then the
  agentmux-tray (Windows tray icon + toast notifications), then the
  Discord bot if discord.toml exists and DISCORD_BOT_TOKEN is set.

  Idempotent: existing tray / Discord processes are detected and left
  alone (their WS subscribers reconnect to the new broker on their own).

  --no-tray       Skip the tray (no icon, no toasts).
  --no-discord    Skip the bot even if configured.
  --foreground    Run broker inline (Ctrl+C to stop). Skips tray and bot;
                  use a separate shell if you need them.
"@
        }
        "stop" {
            @"
Usage: .\agentmux stop

  Stops platform-discord, agentmux-tray, and broker (via the PID
  file under %LOCALAPPDATA%\agentmux\). Safe to run when nothing is
  running — exits cleanly.
"@
        }
        "orchestrator" {
            @"
Usage: .\agentmux orchestrator

  Configures the boss/worker workflow. Sets:
    config.toml::main_session          — which session is the orchestrator
    discord.toml::main_session         — must match (read by the bot)
    discord.toml::worker_thread_parent — channel for auto-spawned worker
                                         threads (0 = post in main's home)
    discord.toml::dashboard_channel_id — channel for the live status panel
                                         (0 = no dashboard)

  Re-runnable. Existing values are shown as defaults; press Enter to keep.

  After running, restart agentmux so broker injects the orchestrator system
  prompt into the named session:
    .\agentmux restart

  See docs/orchestrator-prompt.md for what the prompt teaches the model.
"@
        }
        "restart" {
            @"
Usage: .\agentmux restart [--no-tray] [--no-discord]

  Stops the whole stack (broker + tray + Discord) and starts it again.
  Use after editing config.toml or discord.toml so all three processes
  reload their settings — config changes do not take effect mid-run.

  Same flags as `start` are accepted and forwarded.

  Discord users with /reload and tray users with "Restart agentmux"
  trigger this same flow remotely (broker spawns a detached respawner
  before exiting, then the script re-runs).
"@
        }
        "status" {
            @"
Usage: .\agentmux status

  Prints broker pid + sessions list + Discord adapter state. Read-
  only; no side effects.
"@
        }
        "attach" {
            @"
Usage: .\agentmux attach [name | --new [name] | --session name | --debug]
                        [--broker http://host:port [--token <t>]]

  No args:           interactive picker menu
  <name>:            shorthand for --session <name>
  --new [name]:      create a new session and attach (auto-named s1/s2/.. if omitted)
  --session <name>:  attach directly without the menu
  --debug:           log stdin bytes to stderr (diagnostic)
  --broker <url>:    connect over WebSocket to a remote broker on the LAN
                     (default omits this and uses the local named pipe)
  --token  <token>:  Bearer token for non-loopback brokers
                     (also picked up from `$env:AGENT_ATTACH_TOKEN)

  Browser alternative: open http://<broker>:8765/ — no install needed,
  works on phones/tablets too. Loopback browsers skip the token prompt;
  LAN browsers paste the same token. Auto-reconnect, soft-key bar on
  touch devices.

  Detach with Ctrl+Q or Ctrl+]. Ctrl+C escalation:
    1×           interrupt claude's current turn
    2× in 1.5 s  restart this session's claude (history kept)
    3× in 1.5 s  shut down the entire broker
"@
        }
        "new" {
            @"
Usage: .\agentmux new <name> [-Cwd <path>] [-Persist | -Ephemeral]

  Create a new broker session named <name>. Once created, attach
  with '.\agentmux attach <name>', bind a Discord channel with
  Discord's '!attach <name>', or open the web viewer.

  -Cwd <path>      Working directory the session's claude is spawned
                   in. Defaults to:
                     1. broker config.toml `default_cwd` (if set)
                     2. broker's launch cwd (legacy fallback)
  -Persist         Survive broker restart (auto_resume = true).
  -Ephemeral       Forget on broker restart (auto_resume = false).

  When neither -Persist nor -Ephemeral is given, the broker's
  `auto_resume_default` setting decides.
"@
        }
        "kill" {
            @"
Usage: .\agentmux kill <name> [-Force]

  Delete a session from broker (and from sessions.toml). claude is
  killed if still running. Channel bindings to this session are
  cleared by the Discord bot on next event.

  Default behaviour: shows the session record and asks for confirmation.
  -Force skips the prompt — useful for scripts.

  This is the right command for:
    * cleaning up a 'locally_owned' record you don't plan to re-adopt
    * removing a default session that has the wrong cwd
    * dropping a botched test session
"@
        }
        "adopt" {
            @"
Usage: .\agentmux adopt --resume <claude-session-id> [name] [--cwd <path>]
       .\agentmux adopt <name>

  Bring a claude conversation under broker control. Two forms:

  Fresh adopt (--resume):
    Use after exiting a local 'claude' that you want to keep working
    on remotely. broker spawns 'claude --resume <id>' so the new
    process picks up your transcript.

      name   defaults to the cwd basename
      --cwd  defaults to the current shell's directory

    Get the claude-session-id by running '/status' inside claude
    BEFORE you exit, or by inspecting ~\.claude\projects\<encoded-cwd>\
    after the fact (most-recently-modified .jsonl filename).

    ⚠ Make sure the original claude process has fully exited
      (Ctrl+C / /exit). Two processes on the same session id will
      corrupt the transcript.

  Re-adopt (no --resume, just a name):
    Pulls a previously-demoted session back under broker. Uses the
    claude_session_id stored in sessions.toml.

      ⚠ Exit any local 'claude --resume <id>' you started after
        the demote — same transcript-corruption risk.
"@
        }
        "demote" {
            @"
Usage: .\agentmux demote <name>

  Hand a broker-owned session back to local terminal control. Broker:
    1. injects '/exit\r' into claude's PTY (graceful)
    2. waits up to 2s for clean exit
    3. if still alive, escalates to TerminateProcess + 1s wait
    4. if STILL alive, fails the request and leaves state intact

  On success, prints the exact 'cd ... ; claude --resume <id>' you
  should run in your terminal. The session record stays in broker
  (state=locally_owned) — Discord input to it is refused with a
  hint, and tray shows it greyed out. Re-adopt with:

      .\agentmux adopt <name>

  Channel bindings, name, cwd, claude_session_id are all preserved
  across the round-trip.
"@
        }
        "logs" {
            @"
Usage: .\agentmux logs [broker | discord | tray | events]

  broker  (default)  tail today's broker.YYYY-MM-DD.log
  discord            tail platform-discord.stdout.log
  tray               tail agentmux-tray.stderr.log
  events             tail events.jsonl (audit trail of hook events)

  Live follow (Get-Content -Wait); Ctrl+C to exit.
"@
        }
        "config" {
            @"
Usage: .\agentmux config <subcommand> [args]

  edit  [broker|discord|hooks]    Open in `$env:EDITOR / VS Code / notepad
                                  (notepad blocks; on close auto-runs check)
  dir                             Open %LOCALAPPDATA%\agentmux in Explorer
  path  [broker|discord|hooks]    Print the absolute path (pipe-friendly)
  show  [broker|discord]          Print file contents to stdout
  check [broker|discord|hooks]    Validate (default: all three)
  set   <broker|discord> <key> <value>
                                  Set a scalar field, preserving comments/format
  unset <broker|discord> <key>    Remove a field (falls back to default)
  token [--set]                   Generate a 32-byte URL-safe Bearer token.
                                  Without --set: print to stdout for copying.
                                  With --set:    write to broker.toml's
                                                 attach_token field
                                                 (restart broker to apply).

  Default kind for sub-commands that take one is broker.
"@
        }
        "discord" {
            @"
Usage: .\agentmux discord <subcommand> [args]

  setup                          Interactive: token + channels + users
  token                          Re-prompt + verify the bot token; saves to
                                 User-scope env var (default DISCORD_BOT_TOKEN)
  users    add|remove <id>       Edit allowed_user_ids in discord.toml
  channels add|remove <id>       Edit channel_ids in discord.toml
  start                          Just launch the bot (no other side effects)

  Token is read from env var; never written to disk. allowed_user_ids
  must be non-empty — the bot refuses to start with an empty whitelist.
"@
        }
        "hooks" {
            @"
Usage: .\agentmux hooks <install|uninstall|check>

  install     Wire Stop + Notification + PreToolUse hooks into
              ~\.claude\settings.json. Idempotent; original is backed up
              to settings.json.bak first.
  uninstall   Remove agentmux hook entries (other hooks untouched).
  check       Validate the current hooks setup (paths exist, JSON parses).

  PreToolUse drives the Discord tool-use approval flow. Risky tool calls
  (Bash with rm -rf / curl / sudo, Edit outside the session cwd, …)
  long-poll the broker; the bot prompts you with [✅] / [❌] buttons.
  Safe verbs (Read / Glob / Grep / cargo / git status / …) auto-allow.
"@
        }
        default {
            Write-Host "no verb-level help for `"$verb`" — try .\agentmux help" -ForegroundColor Yellow
            return
        }
    }
    Write-Host ""
}

# Returns $true if Argv requests help (--help, -h, or `help`).
function Wants-Help([string[]]$Argv) {
    if (-not $Argv) { return $false }
    $first = $Argv[0]
    return ($first -eq "--help" -or $first -eq "-h" -or $first -eq "help")
}

function Cmd-Init([string[]]$Argv) {
    if (Wants-Help $Argv) { Show-VerbHelp "init"; return }
    Write-Host ""
    Write-Host "agentmux setup wizard" -ForegroundColor Cyan
    Write-Host "─────────────────────"
    Write-Host ""

    # 1 — prereqs
    Write-Host "[1/6] Checking prerequisites..." -ForegroundColor Cyan
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
    # install-hooks.ps1 is idempotent and dedups by basename, so running
    # it again is always safe — it converges existing entries to a single
    # canonical one pointing at this folder's build. We always offer to
    # run it; the script's own output ("already installed" vs "migrated"
    # vs "consolidated N duplicates") is the source of truth.
    Write-Host "[2/6] Claude Code hooks" -ForegroundColor Cyan
    if (Test-HooksInstalled) {
        Write-Host "  ✓ Existing agentmux hooks detected in $hooksCfg"
        Write-Host "    (re-running is safe — it dedups + repoints to this folder's build)"
        $ans = Read-Host "  Re-run installer to dedup / repoint? [Y/n]"
    } else {
        Write-Host "  Four hooks plug into ~\.claude\settings.json:"
        Write-Host "    Stop         → 'turn complete' events for IM replies / toasts"
        Write-Host "    Notification → permission prompts / idle pings"
        Write-Host "    PreToolUse   → tool-use approval (Discord buttons + tray toast, auto-allows safe verbs)"
        Write-Host "    PostToolUse  → live tool-progress narration in Discord placeholder"
        $ans = Read-Host "  Install hooks now? [Y/n]"
    }
    if ($ans -ne "n" -and $ans -ne "N") {
        & (Join-Path $scriptsDir "install-hooks.ps1")
    }
    Write-Host ""

    # 3 — broker config
    Write-Host "[3/6] Broker configuration" -ForegroundColor Cyan
    if (Test-Path $brokerCfg) {
        Write-Host "  ✓ broker config already exists at $brokerCfg"
    } else {
        $ans = Read-Host "  Write a default broker config.toml? [Y/n]"
        if ($ans -ne "n" -and $ans -ne "N") {
            & (Join-Path $scriptsDir "init-config.ps1")
        }
    }

    # default_cwd is the single most-asked knob: where do new sessions
    # land when you don't pass -Cwd? Without this prompt, users hit the
    # "default session was created in some random subdir" gotcha that
    # shows up in sessions.toml and silently sticks across restarts.
    if (Test-Path $brokerCfg) {
        Require-Binary $agentmuxCli "agentmux-cli.exe"
        # Read the current value to skip the prompt if it's already set.
        $rawCfg = Get-Content -LiteralPath $brokerCfg -Raw -ErrorAction SilentlyContinue
        $hasDefaultCwd = $rawCfg -and ($rawCfg -match '(?m)^\s*default_cwd\s*=\s*"[^"]+"')
        if ($hasDefaultCwd) {
            Write-Host "  ✓ default_cwd already set in $brokerCfg"
        } else {
            Write-Host ""
            Write-Host "  default_cwd controls where new sessions' cwd lands when you" -ForegroundColor Cyan
            Write-Host "  don't pass -Cwd at create time. Empty = each new session inherits" -ForegroundColor Cyan
            Write-Host "  the broker's launch cwd, which is whichever folder you happened" -ForegroundColor Cyan
            Write-Host "  to be in when you ran '.\agentmux start' (a common confusion)." -ForegroundColor Cyan
            $here = (Get-Location).Path
            Write-Host ""
            $suggested = Read-Host "  Set default_cwd? Enter a path (default: '$here'), or '-' to leave unset"
            if ($suggested -ne "-") {
                if (-not $suggested) { $suggested = $here }
                if (Test-Path -LiteralPath $suggested -PathType Container) {
                    $resolved = (Resolve-Path -LiteralPath $suggested).Path
                    & $agentmuxCli config set $brokerCfg default_cwd $resolved | Out-Null
                    Write-Host "  ✓ default_cwd = $resolved" -ForegroundColor Green
                } else {
                    Write-Host "  ⚠ '$suggested' does not exist — skipping" -ForegroundColor Yellow
                }
            }
        }
    }
    Write-Host ""

    # 4 — discord (optional)
    Write-Host "[4/6] Discord IM bridge (optional)" -ForegroundColor Cyan
    $hasDiscordCfg   = Test-Path $discordCfg
    $hasDiscordToken = [bool][Environment]::GetEnvironmentVariable("DISCORD_BOT_TOKEN", "User")
    if ($hasDiscordCfg -and $hasDiscordToken) {
        Write-Host "  ✓ already configured (discord.toml present, DISCORD_BOT_TOKEN set)"
        $ans = Read-Host "  Reconfigure? [y/N]"
    } elseif ($hasDiscordCfg -or $hasDiscordToken) {
        $what = if ($hasDiscordCfg) { "discord.toml present but DISCORD_BOT_TOKEN missing" } `
                else                { "DISCORD_BOT_TOKEN set but discord.toml missing" }
        Write-Host "  ⚠ partial setup detected: $what" -ForegroundColor Yellow
        $ans = Read-Host "  Finish setup? [Y/n]"
        if ($ans -eq "" -or $ans -eq "y" -or $ans -eq "Y") { $ans = "y" }
    } else {
        $ans = Read-Host "  Set up Discord? [y/N]"
    }
    if ($ans -eq "y" -or $ans -eq "Y") {
        Cmd-DiscordSetup
    } else {
        Write-Host "  skipped — re-run later with .\agentmux discord setup"
    }
    Write-Host ""

    # Optional shortcut: switch to the GUI editor for the remaining
    # config knobs. The GUI exposes everything Cmd-OrchestratorSetup
    # asks (and more — Broker / Discord / Hooks / Advanced tabs too).
    $guiExe = $null
    foreach ($p in @(
        (Join-Path $bin "agentmux-config.exe"),
        (Join-Path $root "target\release\agentmux-config.exe"),
        (Join-Path $root "target\debug\agentmux-config.exe")
    )) {
        if (Test-Path $p) { $guiExe = $p; break }
    }
    $useGui = $false
    if ($guiExe) {
        Write-Host "[4.5/6] GUI shortcut (optional)" -ForegroundColor Cyan
        Write-Host "  agentmux-config.exe is built — you can fill in the rest of the"
        Write-Host "  configuration (orchestrator + advanced fields) in a window instead"
        Write-Host "  of CLI prompts."
        $ans = Read-Host "  Open the GUI editor and finish there? [y/N]"
        if ($ans -eq "y" -or $ans -eq "Y") {
            Start-Process -FilePath $guiExe -WindowStyle Hidden | Out-Null
            Write-Host "  opened agentmux config window. Skipping remaining wizard steps."
            $useGui = $true
        }
        Write-Host ""
    }

    if (-not $useGui) {
        # 5 — orchestrator (optional)
        Write-Host "[5/6] Orchestrator workflow (optional)" -ForegroundColor Cyan
        Cmd-OrchestratorSetup
        Write-Host ""
    }

    # 6 — start
    Write-Host "[6/6] Start services" -ForegroundColor Cyan
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
    if (Wants-Help $Argv) { Show-VerbHelp "start"; return }
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

    # Verify broker actually came up before launching tray / discord.
    # Without this guard, a failed broker launch (port collision,
    # pid-file race, etc.) silently leaves tray + discord pointed at
    # a void, AND each subsequent `agentmux start` stacks another
    # discord process. Probe up to 2s; abort the rest of start if
    # broker isn't responding.
    $brokerUp = $false
    for ($i = 0; $i -lt 20; $i++) {
        Start-Sleep -Milliseconds 100
        if (Get-BrokerPid) {
            try {
                $null = Invoke-RestMethod "http://127.0.0.1:8765/sessions" -TimeoutSec 1 -ErrorAction Stop
                $brokerUp = $true
                break
            } catch {}
        }
    }
    if (-not $brokerUp) {
        Write-Host "broker did not come up within 2s — skipping tray and discord" -ForegroundColor Yellow
        Write-Host "  check: .\agentmux logs broker" -ForegroundColor Yellow
        return
    }

    # Tray runs by default — same lifecycle as the broker. Skipped only
    # if --no-tray was passed or the binary isn't built (older release).
    # If a tray is already running, leave it (its WS subscriber will
    # reconnect to the freshly-started broker on its own).
    if (-not ($Argv -contains "--no-tray")) {
        if (Get-Process agentmux-tray -ErrorAction SilentlyContinue) {
            Write-Host "agentmux-tray already running — leaving it (will reconnect to fresh broker)"
        } else {
            $trayExe = (Join-Path $scriptsDir "..\target\release\agentmux-tray.exe")
            $trayBinExe = (Join-Path $scriptsDir "..\bin\agentmux-tray.exe")
            if ((Test-Path $trayExe) -or (Test-Path $trayBinExe)) {
                & (Join-Path $scriptsDir "start-tray.ps1")
            }
        }
    }

    if ($Argv -contains "--no-discord") { return }
    if (-not (Test-Path $discordCfg)) { return }

    # Same idempotent-start logic for the Discord bot — its WS
    # subscriber will reconnect on its own when the broker comes back.
    if (Get-Process platform-discord -ErrorAction SilentlyContinue) {
        Write-Host "platform-discord already running — leaving it (will reconnect to fresh broker)"
        return
    }

    $token = [Environment]::GetEnvironmentVariable("DISCORD_BOT_TOKEN", "User")
    if (-not $token) {
        Write-Host "discord.toml present but DISCORD_BOT_TOKEN env var is empty — bot not started" -ForegroundColor Yellow
        Write-Host "  set with: .\agentmux discord token" -ForegroundColor Yellow
        return
    }
    $env:DISCORD_BOT_TOKEN = $token
    & (Join-Path $scriptsDir "start-discord.ps1")
}

function Cmd-Stop([string[]]$Argv) {
    if (Wants-Help $Argv) { Show-VerbHelp "stop"; return }
    Get-Process -Name platform-discord -ErrorAction SilentlyContinue | ForEach-Object {
        Stop-Process -Id $_.Id -Force
        Write-Host "stopped platform-discord (pid $($_.Id))"
    }
    Get-Process -Name agentmux-tray -ErrorAction SilentlyContinue | ForEach-Object {
        Stop-Process -Id $_.Id -Force
        Write-Host "stopped agentmux-tray (pid $($_.Id))"
    }
    & (Join-Path $scriptsDir "stop-broker.ps1")
}

function Cmd-Restart([string[]]$Argv) {
    if (Wants-Help $Argv) { Show-VerbHelp "restart"; return }
    # Stop everything (broker + discord + tray) then start fresh so all
    # three reload their config from disk. Used after editing config.toml
    # / discord.toml — same effect as `.\agentmux stop; .\agentmux start`.
    Cmd-Stop @()
    # Brief pause so OS handle release (PID file, named pipe, port)
    # finishes before start tries to claim them again.
    Start-Sleep -Milliseconds 500
    Cmd-Start $Argv
}

function Cmd-Status([string[]]$Argv) {
    if (Wants-Help $Argv) { Show-VerbHelp "status"; return }
    $bp = Get-BrokerPid
    if ($bp) {
        Write-Host "broker:  running (pid $bp)" -ForegroundColor Green
        try {
            $sessions = Invoke-RestMethod "http://127.0.0.1:8765/sessions" -TimeoutSec 2
            Write-Host "  sessions: $($sessions.Count)"
            foreach ($s in $sessions) {
                $marker = " "
                $line = "  $marker {0,-18} {1,-14} viewers={2}  cwd={3}" -f $s.name, $s.state, $s.viewers, $s.cwd
                if ($s.state -eq "locally_owned") {
                    Write-Host $line -ForegroundColor Magenta
                } else {
                    Write-Host $line
                }
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

    $tray = Get-Process -Name agentmux-tray -ErrorAction SilentlyContinue
    if ($tray) {
        Write-Host "tray:    running (pid $($tray.Id))" -ForegroundColor Green
    } else {
        Write-Host "tray:    not running" -ForegroundColor Gray
    }
}

function Cmd-Attach([string[]]$Argv) {
    if (Wants-Help $Argv) { Show-VerbHelp "attach"; return }
    Require-Binary $attachExe "claude-attach.exe"
    if ($Argv -and $Argv.Count -gt 0) {
        # Treat first positional as session name unless it already starts with --
        $first = $Argv[0]
        if ($first -and -not $first.StartsWith("--")) {
            & $attachExe --session $first @($Argv[1..($Argv.Count - 1)])
            return
        }
    }
    & $attachExe @Argv
}

function Cmd-Logs([string[]]$Argv) {
    if (Wants-Help $Argv) { Show-VerbHelp "logs"; return }
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
        "tray" {
            $f = Join-Path $dataDir "agentmux-tray.stderr.log"
            if (Test-Path $f) { Get-Content $f -Wait -Tail 30 }
            else { Write-Host "no tray log at $f" }
        }
        "events" {
            $f = Join-Path $dataDir "events.jsonl"
            if (Test-Path $f) { Get-Content $f -Wait -Tail 20 }
            else { Write-Host "no events log at $f" }
        }
        default {
            Write-Host "usage: .\agentmux logs [broker|discord|tray|events]"
        }
    }
}

# --- session create / destroy ------------------------------------------

function Cmd-New([string[]]$Argv) {
    if (Wants-Help $Argv) { Show-VerbHelp "new"; return }

    # Parse: <name?> [-Cwd <path>] [-Persist | -Ephemeral]
    $cwd = $null
    $persist = $null    # $null = leave to broker default; $true / $false = explicit override
    $positional = @()
    $i = 0
    while ($i -lt $Argv.Count) {
        $a = $Argv[$i]
        switch ($a) {
            "-Cwd"        { if ($i + 1 -ge $Argv.Count) { Write-Host "-Cwd needs a path" -ForegroundColor Red; return }; $cwd = $Argv[$i + 1]; $i += 2 }
            "--cwd"       { if ($i + 1 -ge $Argv.Count) { Write-Host "--cwd needs a path" -ForegroundColor Red; return }; $cwd = $Argv[$i + 1]; $i += 2 }
            "-Persist"    { $persist = $true;  $i += 1 }
            "--persist"   { $persist = $true;  $i += 1 }
            "-Ephemeral"  { $persist = $false; $i += 1 }
            "--ephemeral" { $persist = $false; $i += 1 }
            default {
                $positional += $a
                $i += 1
            }
        }
    }

    if ($positional.Count -lt 1) {
        Show-VerbHelp "new"
        return
    }
    $name = $positional[0]
    if ($name -match '[^A-Za-z0-9._-]') {
        Write-Host "✗ session name has invalid chars (use only letters, digits, . _ -)" -ForegroundColor Red
        return
    }

    $body = @{ name = $name }
    if ($cwd) {
        if (-not (Test-Path -LiteralPath $cwd -PathType Container)) {
            Write-Host "✗ cwd does not exist: $cwd" -ForegroundColor Red
            return
        }
        $body.cwd = (Resolve-Path -LiteralPath $cwd).Path
    }
    if ($null -ne $persist) { $body.auto_resume = $persist }

    $r = Invoke-BrokerJson -Method POST -Path "/sessions" -Body $body
    if (-not $r.ok) {
        if ($r.status -eq 409) {
            Write-Host "✗ a session named '$name' already exists." -ForegroundColor Red
            Write-Host "  $($r.body)"
        } elseif ($r.status -eq 400) {
            Write-Host "✗ broker rejected request: $($r.body)" -ForegroundColor Red
        } else {
            Write-Host "✗ broker error: HTTP $($r.status) $($r.body)" -ForegroundColor Red
            if (-not $r.status) { Write-Host "  ($($r.err))" -ForegroundColor Red }
        }
        return
    }
    $persistLabel = if ($r.data.auto_resume) { "persisted" } else { "ephemeral" }
    Write-Host "✓ created '$($r.data.name)' (id=$($r.data.id), $persistLabel)" -ForegroundColor Green
    Write-Host "  cwd: $($r.data.cwd)"
    Write-Host ""
    Write-Host "Next:"
    Write-Host "  .\agentmux attach $name"
}

function Cmd-Kill([string[]]$Argv) {
    if (Wants-Help $Argv -or -not $Argv -or $Argv.Count -lt 1) { Show-VerbHelp "kill"; return }
    $name = $Argv[0]
    $force = ($Argv -contains "-Force") -or ($Argv -contains "--force")

    if (-not $force) {
        # Show what will be deleted so the user can back out.
        $g = Invoke-BrokerJson -Method GET -Path "/sessions/$name"
        if (-not $g.ok) {
            if ($g.status -eq 404) {
                Write-Host "no session named '$name'." -ForegroundColor Yellow
            } else {
                Write-Host "✗ broker query failed: HTTP $($g.status) $($g.body)" -ForegroundColor Red
            }
            return
        }
        Write-Host "Will delete:"
        Write-Host "  name              : $($g.data.name)"
        Write-Host "  cwd               : $($g.data.cwd)"
        Write-Host "  state             : $($g.data.state)"
        Write-Host "  claude session id : $($g.data.claude_session_id)"
        Write-Host ""
        $ans = Read-Host "Proceed? [y/N]"
        if ($ans -ne "y" -and $ans -ne "Y") { Write-Host "aborted."; return }
    }

    $r = Invoke-BrokerJson -Method DELETE -Path "/sessions/${name}?force=true"
    if (-not $r.ok) {
        Write-Host "✗ kill failed: HTTP $($r.status) $($r.body)" -ForegroundColor Red
        return
    }
    Write-Host "✓ session '$name' deleted." -ForegroundColor Green
}

# --- adopt / demote -----------------------------------------------------

function Cmd-Adopt([string[]]$Argv) {
    if (Wants-Help $Argv) { Show-VerbHelp "adopt"; return }

    # Parse: --resume <id> [name] [--cwd <path>]    (fresh adopt)
    #     OR <name>                                  (re-adopt LocallyOwned)
    $resumeId = $null
    $cwd = $null
    $positional = @()
    $i = 0
    while ($i -lt $Argv.Count) {
        $a = $Argv[$i]
        switch ($a) {
            "--resume" {
                if ($i + 1 -ge $Argv.Count) { Write-Host "--resume requires a claude session id" -ForegroundColor Red; return }
                $resumeId = $Argv[$i + 1]
                $i += 2
            }
            "--cwd" {
                if ($i + 1 -ge $Argv.Count) { Write-Host "--cwd requires a path" -ForegroundColor Red; return }
                $cwd = $Argv[$i + 1]
                $i += 2
            }
            default {
                $positional += $a
                $i += 1
            }
        }
    }

    if ($resumeId) {
        # ---- fresh adopt ------------------------------------------------
        if (-not [System.Guid]::TryParse($resumeId, [ref][System.Guid]::Empty)) {
            Write-Host "✗ --resume value doesn't look like a claude session id (UUID)" -ForegroundColor Red
            Write-Host "  Get the id from claude's /status command before exiting, or"
            Write-Host "  pick the most recent jsonl in ~\.claude\projects\<encoded-cwd>\"
            return
        }
        if (-not $cwd) { $cwd = (Get-Location).Path }
        if (-not (Test-Path -LiteralPath $cwd -PathType Container)) {
            Write-Host "✗ cwd does not exist: $cwd" -ForegroundColor Red
            return
        }
        $cwd = (Resolve-Path -LiteralPath $cwd).Path

        $name = if ($positional.Count -ge 1) { $positional[0] } else { Split-Path -Leaf $cwd }
        # Sanitise: name maps to a Discord channel binding key, must be filesystem-safe.
        $name = ($name -replace '[^A-Za-z0-9._-]', '-')
        if (-not $name) { Write-Host "✗ could not derive a session name from cwd" -ForegroundColor Red; return }

        Write-Host ""
        Write-Host "Fresh adopt:" -ForegroundColor Cyan
        Write-Host "  name              : $name"
        Write-Host "  cwd               : $cwd"
        Write-Host "  claude session id : $resumeId"
        Write-Host ""
        Write-Host "  Make sure the original 'claude' (the one with this session id) has fully exited" -ForegroundColor Yellow
        Write-Host "  in the other terminal — two ` --resume `s on the same id will corrupt the transcript." -ForegroundColor Yellow
        $ans = Read-Host "Proceed? [Y/n]"
        if ($ans -eq "n" -or $ans -eq "N") { Write-Host "aborted."; return }

        $r = Invoke-BrokerJson -Method POST -Path "/sessions" -Body @{
            name = $name
            cwd = $cwd
            resume_session_id = $resumeId
            auto_resume = $true
        }
        if (-not $r.ok) {
            Write-Host "✗ broker rejected adopt:" -ForegroundColor Red
            if ($r.status -eq 409) {
                Write-Host "  $($r.body)" -ForegroundColor Red
                Write-Host "  Hint: a session with that name already exists. Either re-adopt with"
                Write-Host "        '.\agentmux adopt $name' (if it's locally-owned) or pick a"
                Write-Host "        different name."
            } else {
                Write-Host "  HTTP $($r.status): $($r.body)" -ForegroundColor Red
                if (-not $r.status) { Write-Host "  ($($r.err))" -ForegroundColor Red }
            }
            return
        }
        Write-Host "✓ session '$($r.data.name)' adopted under broker (id=$($r.data.id))" -ForegroundColor Green
        Write-Host ""
        Write-Host "Next steps:"
        Write-Host "  .\agentmux attach $name        # local terminal viewer"
        Write-Host "  open Discord, !attach $name    # bind a channel to it"
        Write-Host "  http://<broker>:8765/          # browser viewer"
        return
    }

    # ---- re-adopt ----------------------------------------------------
    if ($positional.Count -lt 1) {
        Show-VerbHelp "adopt"
        return
    }
    $name = $positional[0]

    $g = Invoke-BrokerJson -Method GET -Path "/sessions/$name"
    if (-not $g.ok) {
        if ($g.status -eq 404) {
            Write-Host "✗ no broker session named '$name'." -ForegroundColor Red
            Write-Host "  To do a fresh adopt of an external claude:" -ForegroundColor Yellow
            Write-Host "    .\agentmux adopt --resume <claude-session-id> [name]" -ForegroundColor Yellow
        } else {
            Write-Host "✗ broker query failed: HTTP $($g.status) $($g.body)" -ForegroundColor Red
        }
        return
    }
    if ($g.data.state -ne "locally_owned") {
        Write-Host "✗ session '$name' is in state '$($g.data.state)', not locally_owned." -ForegroundColor Red
        Write-Host "  Nothing to re-adopt — the broker already owns this session."
        return
    }

    Write-Host ""
    Write-Host "Re-adopt:" -ForegroundColor Cyan
    Write-Host "  name              : $($g.data.name)"
    Write-Host "  cwd               : $($g.data.cwd)"
    Write-Host "  claude session id : $($g.data.claude_session_id)"
    Write-Host ""
    Write-Host "  ⚠ Make sure your local 'claude --resume $($g.data.claude_session_id)' has" -ForegroundColor Yellow
    Write-Host "    been exited (Ctrl+C / /exit). Two processes on the same session id will" -ForegroundColor Yellow
    Write-Host "    corrupt the transcript." -ForegroundColor Yellow
    $ans = Read-Host "Proceed? [Y/n]"
    if ($ans -eq "n" -or $ans -eq "N") { Write-Host "aborted."; return }

    $r = Invoke-BrokerJson -Method POST -Path "/sessions/$name/adopt"
    if (-not $r.ok) {
        Write-Host "✗ re-adopt failed: HTTP $($r.status)" -ForegroundColor Red
        Write-Host "  $($r.body)" -ForegroundColor Red
        return
    }
    Write-Host "✓ session '$($r.data.name)' is back under broker (state=$($r.data.state))" -ForegroundColor Green
    Write-Host ""
    Write-Host "Next steps:"
    Write-Host "  .\agentmux attach $name"
}

function Cmd-Demote([string[]]$Argv) {
    if (Wants-Help $Argv -or -not $Argv -or $Argv.Count -lt 1) { Show-VerbHelp "demote"; return }
    $name = $Argv[0]

    $g = Invoke-BrokerJson -Method GET -Path "/sessions/$name"
    if (-not $g.ok) {
        if ($g.status -eq 404) {
            Write-Host "✗ no broker session named '$name'." -ForegroundColor Red
        } else {
            Write-Host "✗ broker query failed: HTTP $($g.status) $($g.body)" -ForegroundColor Red
        }
        return
    }
    if ($g.data.state -eq "locally_owned") {
        Write-Host "session '$name' is already locally-owned — nothing to do." -ForegroundColor Yellow
        Write-Host ""
        Write-Host "Resume command (copy this whole line):"
        $cwdEsc = $g.data.cwd
        if ($g.data.claude_session_id) {
            Write-Host "  cd `"$cwdEsc`" ; claude --resume $($g.data.claude_session_id)" -ForegroundColor Cyan
        } else {
            Write-Host "  cd `"$cwdEsc`" ; claude   # no claude_session_id recorded — start fresh" -ForegroundColor Cyan
        }
        return
    }
    if (-not $g.data.claude_session_id) {
        Write-Host "⚠ session '$name' has no recorded claude_session_id yet." -ForegroundColor Yellow
        Write-Host "  Demote will still kill claude, but you won't be able to --resume locally."
        Write-Host "  This usually means the session never completed a turn under broker."
        $ans = Read-Host "Proceed anyway? [y/N]"
        if ($ans -ne "y" -and $ans -ne "Y") { Write-Host "aborted."; return }
    }

    # Wide timeout: graceful /exit + kill + wait can take ~3 s, plus
    # any blocking that comes from the broker side. 30s is generous.
    $r = Invoke-BrokerJson -Method POST -Path "/sessions/$name/demote" -TimeoutSec 30
    if (-not $r.ok) {
        Write-Host "✗ demote failed: HTTP $($r.status)" -ForegroundColor Red
        Write-Host "  $($r.body)" -ForegroundColor Red
        if ($r.status -eq 500) {
            Write-Host ""
            Write-Host "  This means broker couldn't kill claude even after TerminateProcess." -ForegroundColor Yellow
            Write-Host "  Investigate via Task Manager. The session was NOT transitioned to" -ForegroundColor Yellow
            Write-Host "  locally-owned, so it's safe to retry once claude is gone." -ForegroundColor Yellow
        }
        return
    }
    Write-Host "✓ session '$name' demoted (graceful=$($r.data.graceful))" -ForegroundColor Green
    if (-not $r.data.graceful) {
        Write-Host "  ⚠ /exit window timed out, broker had to TerminateProcess." -ForegroundColor Yellow
        Write-Host "    Last few transcript lines may not have been flushed." -ForegroundColor Yellow
    }
    Write-Host ""
    # Print as a single line — `cd` and `claude --resume` MUST run in
    # the same shell for claude to find the transcript (it looks under
    # ~\.claude\projects\<encoded-cwd>\<id>.jsonl). Splitting them
    # across two Write-Host lines invited copy-paste-just-the-second-
    # line confusion. Single line, copyable.
    Write-Host "Resume in your terminal (copy this whole line):"
    if ($r.data.claude_session_id) {
        Write-Host "  cd `"$($r.data.cwd)`" ; claude --resume $($r.data.claude_session_id)" -ForegroundColor Cyan
    } else {
        Write-Host "  cd `"$($r.data.cwd)`" ; claude   # no claude_session_id recorded — start fresh" -ForegroundColor Cyan
    }
    Write-Host ""
    Write-Host "When you want broker to take over again:"
    Write-Host "  .\agentmux adopt $name"
}

# --- config subcommands -------------------------------------------------

function Cmd-Config([string[]]$Argv) {
    if (-not $Argv -or $Argv.Count -eq 0 -or (Wants-Help $Argv)) {
        Show-VerbHelp "config"
        return
    }
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
        "token"  { Cmd-ConfigToken $rest }
        "gui"    { Cmd-ConfigGui }
        default  {
            Write-Host "unknown config subcommand: $sub" -ForegroundColor Red
            Write-Host ""
            Show-VerbHelp "config"
        }
    }
}

function Cmd-ConfigGui {
    # Spawn the eframe GUI editor as a detached child. The binary lives
    # next to the other release artifacts (bin/ on a release zip,
    # target/release / target/debug for cargo builds). Independent
    # process so a GUI crash can't take down the calling shell.
    $candidates = @(
        (Join-Path $bin "agentmux-config.exe"),
        (Join-Path $root "target\release\agentmux-config.exe"),
        (Join-Path $root "target\debug\agentmux-config.exe")
    )
    $exe = $candidates | Where-Object { Test-Path $_ } | Select-Object -First 1
    if (-not $exe) {
        Write-Host "✗ agentmux-config.exe not found." -ForegroundColor Red
        Write-Host "  Build it with:  cargo build --release -p agentmux-config" -ForegroundColor Yellow
        return
    }
    Start-Process -FilePath $exe -WindowStyle Hidden | Out-Null
    Write-Host "opened agentmux config window."
}

function Cmd-ConfigToken([string[]]$Argv) {
    # Generate 32 cryptographically-random bytes, URL-safe base64 (no
    # padding) so the result is shell-safe. With --set, write directly
    # to the broker config.toml's `attach_token` field.
    $bytes = New-Object byte[] 32
    [System.Security.Cryptography.RandomNumberGenerator]::Create().GetBytes($bytes)
    $token = [Convert]::ToBase64String($bytes).TrimEnd('=').Replace('+','-').Replace('/','_')
    $set = $Argv -and ($Argv -contains '--set')
    if ($set) {
        Require-Binary $agentmuxCli "agentmux-cli.exe"
        if (-not (Test-Path $brokerCfg)) {
            Write-Host "broker config does not exist — creating from template" -ForegroundColor Yellow
            & (Join-Path $scriptsDir "init-config.ps1")
        }
        & $agentmuxCli config set $brokerCfg attach_token $token | Out-Null
        Write-Host "✓ wrote attach_token to $brokerCfg" -ForegroundColor Green
        Write-Host "  Restart broker for the change to take effect:" -ForegroundColor Yellow
        Write-Host "    .\agentmux stop && .\agentmux start"
        Write-Host ""
        Write-Host "Use this token from a remote viewer:"
        Write-Host "  `$env:AGENT_ATTACH_TOKEN = '$token'"
        Write-Host "  claude-attach.exe --broker http://<broker-host>:8765 --session default"
    } else {
        Write-Output $token
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
    if (-not $Argv -or $Argv.Count -eq 0 -or (Wants-Help $Argv)) {
        Show-VerbHelp "discord"
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
        default    {
            Write-Host "unknown discord subcommand: $sub" -ForegroundColor Red
            Write-Host ""
            Show-VerbHelp "discord"
        }
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

function Cmd-OrchestratorSetup {
    # Orchestrator workflow — one session ("main") receives @-mentions
    # in Discord, decides whether to dispatch work to other sessions
    # (workers), and reports back. Workers get their own Discord thread.
    # A dashboard channel shows all sessions' live status.
    #
    # Three knobs all live in the existing config files:
    #   config.toml::main_session                 — the orchestrator session
    #   discord.toml::main_session                — same value (must match)
    #   discord.toml::worker_thread_parent        — channel under which
    #                                               worker threads spawn
    #   discord.toml::dashboard_channel_id        — where the status panel
    #                                               lives as a single message
    #
    # All optional and additive: skipping leaves the bot in plain
    # "every channel binds 1:1 to a session" mode (the pre-0.3.4 UX).
    Require-Binary $agentmuxCli "agentmux-cli.exe"

    if (-not (Test-Path $brokerCfg)) {
        Write-Host "  ⚠ no broker config.toml at $brokerCfg" -ForegroundColor Yellow
        Write-Host "    skipping orchestrator setup; run [3/6] broker config first."
        return
    }
    $hasDiscord = Test-Path $discordCfg

    # Read current values so we can show them and skip prompts where set.
    $rawBroker  = Get-Content -LiteralPath $brokerCfg  -Raw -ErrorAction SilentlyContinue
    $rawDiscord = if ($hasDiscord) { Get-Content -LiteralPath $discordCfg -Raw -ErrorAction SilentlyContinue } else { "" }

    $currentMain    = $null
    $currentParent  = $null
    $currentDash    = $null
    if ($rawBroker  -match '(?m)^\s*main_session\s*=\s*"([^"]*)"')          { $currentMain   = $matches[1] }
    if ($rawDiscord -match '(?m)^\s*worker_thread_parent\s*=\s*(\d+)')      { $currentParent = $matches[1] }
    if ($rawDiscord -match '(?m)^\s*dashboard_channel_id\s*=\s*(\d+)')      { $currentDash   = $matches[1] }

    Write-Host ""
    Write-Host "  The orchestrator pattern lets one 'main' session decide which" -ForegroundColor Cyan
    Write-Host "  other sessions handle which tasks. Workers get their own Discord" -ForegroundColor Cyan
    Write-Host "  thread; a dashboard channel shows everyone's live status." -ForegroundColor Cyan
    Write-Host "  See docs/orchestrator-prompt.md for the full role spec."         -ForegroundColor Cyan
    Write-Host ""

    if ($currentMain) {
        Write-Host "  current main_session: $currentMain" -ForegroundColor Green
        if ($currentParent) { Write-Host "  current worker_thread_parent: $currentParent" -ForegroundColor Green }
        if ($currentDash)   { Write-Host "  current dashboard_channel_id: $currentDash"   -ForegroundColor Green }
        $ans = Read-Host "  Reconfigure orchestrator? [y/N]"
    } else {
        $ans = Read-Host "  Enable orchestrator workflow? [y/N]"
    }
    if ($ans -ne "y" -and $ans -ne "Y") {
        Write-Host "  skipped — re-run later with .\agentmux init or edit the config files directly"
        return
    }

    # main_session — required. Defaults to "default" since that session
    # always exists. Empty string disables on the broker side.
    $defaultMain = if ($currentMain) { $currentMain } else { "default" }
    $mainName = Read-Host "    main session name [$defaultMain]"
    if (-not $mainName) { $mainName = $defaultMain }
    if ($mainName -match '\s') {
        Write-Host "  ⚠ session names cannot contain whitespace — aborting" -ForegroundColor Yellow
        return
    }

    & $agentmuxCli config set $brokerCfg main_session $mainName | Out-Null
    Write-Host "    ✓ config.toml main_session = $mainName" -ForegroundColor Green

    if ($hasDiscord) {
        & $agentmuxCli config set $discordCfg main_session $mainName | Out-Null
        Write-Host "    ✓ discord.toml main_session = $mainName" -ForegroundColor Green

        # Orchestrator's "@bot in any channel routes to main" only works
        # when `respond_to_mentions` is true — otherwise the bot ignores
        # @-mentions in non-whitelisted channels and the entry point
        # silently does nothing. Detect-and-fix automatically; without
        # this auto-flip the user has to discover the dependency by
        # hitting the bug.
        $rawDiscord2 = Get-Content -LiteralPath $discordCfg -Raw -ErrorAction SilentlyContinue
        $respondsAlready = $rawDiscord2 -match '(?m)^\s*respond_to_mentions\s*=\s*true'
        if (-not $respondsAlready) {
            & $agentmuxCli config set $discordCfg respond_to_mentions true | Out-Null
            Write-Host "    ✓ discord.toml respond_to_mentions = true (required for @-mention routing)" -ForegroundColor Green
        }

        Write-Host ""
        Write-Host "    worker_thread_parent: a Discord channel under which the bot" -ForegroundColor Cyan
        Write-Host "    creates a thread for every spawned worker. Right-click the" -ForegroundColor Cyan
        Write-Host "    channel → Copy ID. Empty = workers post in main's home channel." -ForegroundColor Cyan
        $defaultParent = if ($currentParent) { $currentParent } else { "-" }
        $parentInput = Read-Host "    worker_thread_parent channel id [$defaultParent for unset]"
        if (-not $parentInput) { $parentInput = $defaultParent }
        if ($parentInput -ne "-") {
            if ($parentInput -match '^\d{17,20}$') {
                & $agentmuxCli config set $discordCfg worker_thread_parent $parentInput | Out-Null
                Write-Host "    ✓ discord.toml worker_thread_parent = $parentInput" -ForegroundColor Green
            } else {
                Write-Host "    ⚠ '$parentInput' doesn't look like a Discord channel id — skipping" -ForegroundColor Yellow
            }
        } else {
            & $agentmuxCli config set $discordCfg worker_thread_parent 0 | Out-Null
            Write-Host "    ✓ discord.toml worker_thread_parent = 0 (no auto-thread)"
        }

        Write-Host ""
        Write-Host "    dashboard_channel_id: a Discord channel where the bot maintains" -ForegroundColor Cyan
        Write-Host "    a single embed listing every session and its current status." -ForegroundColor Cyan
        Write-Host "    Updated every ~5 s. Empty = no dashboard."                     -ForegroundColor Cyan
        $defaultDash = if ($currentDash) { $currentDash } else { "-" }
        $dashInput = Read-Host "    dashboard_channel_id [$defaultDash for unset]"
        if (-not $dashInput) { $dashInput = $defaultDash }
        if ($dashInput -ne "-") {
            if ($dashInput -match '^\d{17,20}$') {
                & $agentmuxCli config set $discordCfg dashboard_channel_id $dashInput | Out-Null
                Write-Host "    ✓ discord.toml dashboard_channel_id = $dashInput" -ForegroundColor Green
            } else {
                Write-Host "    ⚠ '$dashInput' doesn't look like a Discord channel id — skipping" -ForegroundColor Yellow
            }
        } else {
            & $agentmuxCli config set $discordCfg dashboard_channel_id 0 | Out-Null
            Write-Host "    ✓ discord.toml dashboard_channel_id = 0 (no dashboard)"
        }
    } else {
        Write-Host ""
        Write-Host "    (Discord not configured — skipping discord-side knobs."        -ForegroundColor Yellow
        Write-Host "     The bootstrap prompt still injects, so the main session"     -ForegroundColor Yellow
        Write-Host "     will know how to dispatch via curl from a terminal viewer.)" -ForegroundColor Yellow
    }

    Write-Host ""
    Write-Host "  ✓ orchestrator configured. Restart with .\agentmux restart for" -ForegroundColor Green
    Write-Host "    broker to inject the orchestrator system prompt into '$mainName'." -ForegroundColor Green
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
    if (-not $Argv -or $Argv.Count -eq 0 -or (Wants-Help $Argv)) {
        Show-VerbHelp "hooks"
        return
    }
    switch ($Argv[0]) {
        "install"   { & (Join-Path $scriptsDir "install-hooks.ps1") }
        "uninstall" { & (Join-Path $scriptsDir "install-hooks.ps1") -Uninstall }
        "check"     { Cmd-ConfigCheck @("hooks") }
        default     {
            Write-Host "unknown hooks subcommand: $($Argv[0])" -ForegroundColor Red
            Write-Host ""
            Show-VerbHelp "hooks"
        }
    }
}

# --- dispatch -----------------------------------------------------------

function Dispatch-Help([string[]]$Argv) {
    if ($Argv -and $Argv.Count -gt 0) {
        Show-VerbHelp $Argv[0]
    } else {
        Cmd-Help
    }
}

switch ($Command) {
    ""        { Cmd-Help }
    "help"    { Dispatch-Help $Rest }
    "--help"  { Dispatch-Help $Rest }
    "-h"      { Dispatch-Help $Rest }

    "init"    { Cmd-Init    $Rest }
    "start"   { Cmd-Start   $Rest }
    "stop"    { Cmd-Stop    $Rest }
    "restart" { Cmd-Restart $Rest }
    "status"  { Cmd-Status  $Rest }
    "attach"  { Cmd-Attach  $Rest }
    "logs"    { Cmd-Logs    $Rest }

    "config"  { Cmd-Config  $Rest }
    "discord" { Cmd-Discord $Rest }
    "hooks"   { Cmd-Hooks   $Rest }
    "orchestrator" {
        if (Wants-Help $Rest) { Show-VerbHelp "orchestrator"; return }
        Cmd-OrchestratorSetup
    }

    "new"     { Cmd-New     $Rest }
    "kill"    { Cmd-Kill    $Rest }
    "adopt"   { Cmd-Adopt   $Rest }
    "demote"  { Cmd-Demote  $Rest }

    default {
        Write-Host "unknown command: $Command" -ForegroundColor Red
        Write-Host ""
        Cmd-Help
        exit 1
    }
}
