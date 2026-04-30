# agentmux — Quickstart

## What you need

- **Windows 10/11** for the full feature set, **or Linux x86_64 / macOS**
  (Apple Silicon or Intel) for everything except the system-tray +
  Windows toast (which is Windows-only). The Unix builds are intended
  for headless servers and dev laptops — broker + Discord bot + browser
  web viewer + remote `claude-attach` over SSH/LAN.
- **[Claude Code CLI](https://docs.anthropic.com/en/docs/claude-code)** on `PATH` — install with `npm install -g @anthropic-ai/claude-code`

## Install

### Windows

Download `agentmux-vX.Y.Z-windows-x86_64.zip` from the [releases page](https://github.com/anthropics/agentmux/releases) and extract it anywhere — e.g. `C:\Tools\agentmux\`.

That folder is self-contained; no installer, no PATH changes, no admin rights needed.

### Linux / macOS

Download the matching tarball from the [releases page](https://github.com/anthropics/agentmux/releases):

- Linux x86_64 → `agentmux-vX.Y.Z-linux-x86_64.tar.gz`
- Apple Silicon Mac → `agentmux-vX.Y.Z-macos-aarch64.tar.gz`
- Intel Mac → `agentmux-vX.Y.Z-macos-x86_64.tar.gz`

Extract:

```bash
tar -xzf agentmux-v*-*.tar.gz
cd agentmux-v*-*
ls bin/   # broker, claude-attach, hook-stop, hook-notification, hook-pretool,
          # hook-posttool, platform-discord, agentmux-cli
```

The folder is self-contained; the Linux binaries link only against
glibc, the macOS binaries only against system libraries shipped with
the OS. **macOS users:** the first launch may be blocked by Gatekeeper
because the binaries aren't notarised — clear the quarantine attribute
with `xattr -dr com.apple.quarantine bin/` or right-click → Open in
Finder once.

## Set up

### Windows (interactive wizard)

In a PowerShell window opened in the extracted folder:

```powershell
.\agentmux init
```

The wizard walks you through:

1. **Prerequisite check** — confirms binaries are present and `claude` is on PATH
2. **Hooks** — wires Claude Code's `Stop`, `Notification`, `PreToolUse`, and `PostToolUse` hooks into `~\.claude\settings.json` so agentmux can see turn completions, permission prompts, live tool-progress narration, and (optionally) ask you to approve risky tool calls. Idempotent; the wizard re-detects already-installed hooks and dedups any leftover entries from prior installs
3. **Broker config** — writes `%LOCALAPPDATA%\agentmux\config.toml` if missing (all defaults; you'll rarely need to edit)
4. **Discord IM** *(optional)* — prompts for bot token, channel ID, and your user ID. The token is verified against Discord before being saved to a User-scope environment variable. Wizard re-detects an already-configured Discord and skips
5. **Start broker** — launches the daemon, the system-tray icon, and (if configured) the Discord bot, all as detached background processes

### Linux / macOS (manual setup — `init` wrapper not yet ported)

```bash
# data dir — Linux: ~/.local/share/agentmux/; macOS: ~/Library/Application Support/agentmux/
case "$(uname -s)" in
  Darwin) DATA_DIR="$HOME/Library/Application Support/agentmux" ;;
  *)      DATA_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/agentmux" ;;
esac
mkdir -p "$DATA_DIR"
ROOT="$(pwd)/bin"                               # absolute path to bin/

# 1. Wire the hooks. Edit ~/.claude/settings.json and add (merge with
#    any existing "hooks" key):
cat <<EOF
{
  "hooks": {
    "Stop":         [{"hooks": [{"type":"command","command":"$ROOT/hook-stop"}]}],
    "Notification": [{"hooks": [{"type":"command","command":"$ROOT/hook-notification"}]}],
    "PreToolUse":   [{"matcher":"*","hooks":[{"type":"command","command":"$ROOT/hook-pretool"}]}],
    "PostToolUse":  [{"matcher":"*","hooks":[{"type":"command","command":"$ROOT/hook-posttool"}]}]
  }
}
EOF

# 2. (Optional) Discord — set the bot token in your shell rc:
#       export DISCORD_BOT_TOKEN='xxx'
#    and write ~/.local/share/agentmux/discord.toml. See the
#    "Discord IM bridge" section below for fields.

# 3. Launch broker (foreground for first run; ^C exits):
$ROOT/broker

# Or as a background service via systemd (sample unit):
#   ~/.config/systemd/user/agentmux-broker.service
#       [Unit]
#       Description=agentmux broker
#       [Service]
#       ExecStart=%h/agentmux/bin/broker
#       Restart=on-failure
#       [Install]
#       WantedBy=default.target
# Then: systemctl --user daemon-reload && systemctl --user enable --now agentmux-broker
```

Linux config / state lives under `~/.local/share/agentmux/`; macOS
under `~/Library/Application Support/agentmux/` (`config.toml`,
`sessions.toml`, `discord.toml`, `logs/`, …) — the same data as
`%LOCALAPPDATA%\agentmux\` on Windows, just at the platform-native
location.

The system-tray + toast surfaces are Windows-only. On Linux / macOS,
use the **browser web viewer** at `http://<broker>:8765/` (works from
any device, including phones) for at-a-glance access, and rely on
**Discord** for tool-approval prompts when you're away from the
machine.

---

After that, you have a running agentmux. On Windows, **look at the system
tray (bottom-right of the taskbar)** — a small coloured circle is the
agentmux tray icon. Right-click for the per-session menu. Or, on either
platform:

```powershell
.\agentmux attach            # Windows: enter claude's TUI
.\agentmux status            # Windows: see what's running
.\agentmux help              # Windows: full command list
```

```bash
./bin/claude-attach          # Linux / macOS: enter claude's TUI
ps aux | grep agentmux       # Linux / macOS: see what's running (no wrapper yet)
./bin/claude-attach --help   # Linux / macOS: viewer flags
```

## Daily ops cheat sheet

The PowerShell `.\agentmux <verb>` wrapper is Windows-only. The
Linux / macOS column below shows the direct binary invocations.

```powershell
.\agentmux start             # broker + tray + Discord bot (if configured)
.\agentmux stop              # all of the above
.\agentmux status            # one-line health summary
.\agentmux logs broker       # also: discord, tray, events
.\agentmux logs discord
.\agentmux logs tray
.\agentmux logs events       # events.jsonl audit trail

.\agentmux attach            # picker menu of sessions
.\agentmux attach default    # attach directly
.\agentmux attach --new foo  # create + attach a new session

.\agentmux new blog -Cwd G:\projects\blog -Persist
                             # create a session in another directory
.\agentmux kill blog         # delete a session record (asks for confirmation)
.\agentmux demote default    # hand a session back to local terminal
.\agentmux adopt default     # bring a demoted session back under broker
```

```bash
# Linux / macOS equivalents — invoke binaries directly. Most
# session-lifecycle verbs (new / kill / demote / adopt / persist /
# interrupt / restart / hibernate) are exposed over the broker's
# HTTP control plane:
#
curl -s http://127.0.0.1:8765/sessions | jq                    # list
curl -s http://127.0.0.1:8765/sessions/default/state           # one session
curl -sX POST -H "Content-Type: application/json" \
  -d '{"name":"blog","cwd":"/home/me/projects/blog","auto_resume":true}' \
  http://127.0.0.1:8765/sessions                               # create
curl -sX POST http://127.0.0.1:8765/sessions/blog/interrupt    # ^C
curl -sX DELETE http://127.0.0.1:8765/sessions/blog?force=true # kill
curl -sX POST http://127.0.0.1:8765/shutdown                   # broker stop
#
# Attach a viewer:
./bin/claude-attach                          # session menu
./bin/claude-attach --session default        # direct
./bin/claude-attach --new blog               # create + attach
./bin/claude-attach --broker http://host:8765 \
                    --token "$AGENT_ATTACH_TOKEN"  # remote (LAN)
#
# Logs (daily-rotated under <data-dir>/logs/):
case "$(uname -s)" in
  Darwin) LOGS="$HOME/Library/Application Support/agentmux" ;;
  *)      LOGS="${XDG_DATA_HOME:-$HOME/.local/share}/agentmux" ;;
esac
tail -f "$LOGS/logs/broker.$(date +%F).log"
tail -f "$LOGS/events.$(date +%F).jsonl"                       # audit trail
```

For the **local→broker handover** scenario (started a session locally,
now want to keep working remotely), see *Take over a local claude
conversation* below.

`--no-tray` and `--no-discord` flags on `start` opt out of those processes
respectively (the broker is always started).

Inside `claude-attach`:

| Keys | Action |
|---|---|
| Ctrl+Q or Ctrl+] | Detach (broker keeps running) |
| Ctrl+C ×1 | Interrupt claude's current turn |
| Ctrl+C ×2 within 1.5 s | Restart claude in this session (history preserved) |
| Ctrl+C ×3 within 1.5 s | ⚠️ Shut down the entire broker |

## Discord — what to do once the bot is online

In any whitelisted channel:

```
hello                       → forward to this channel's bound session (lazy-binds to "default" the first time)
!attach <name>              → switch this channel's binding (or use /attach with autocomplete)
!new myproj -cwd D:\repos\x → create a new session and bind this channel to it
!ls                         → list sessions, see which channel is bound to which session
!cwd                        → show the bound session's working directory
!logs [n]                   → last n lines of the session's TUI output (max 100)
!persist on | off           → make this session survive broker restart (default: ephemeral)
!interrupt | !restart | !hibernate
!kill <name>                → destroy a session
!help                       → all of the above
```

**React on any bot message with 🛑 / 💤 / 🔄** to interrupt / hibernate / restart that session — no typing needed.

**Reply** (Discord's UI) to a bot message to forward your new turn to *that* session, regardless of the channel binding.

**Drop an image** into the chat with a caption and the bot saves it locally and tells claude to read it via the `Read` tool.

### Tweaking Discord settings later

```powershell
.\agentmux discord setup                  # add channels/users interactively
.\agentmux discord token                  # rotate the bot token (verified live)
.\agentmux discord users add <id>         # whitelist a user
.\agentmux discord channels remove <id>   # un-whitelist a channel
```

A few config flags worth knowing about (in `discord.toml`):

| Flag | Default | Effect |
|---|---|---|
| `allow_dm` | `false` | Accept 1:1 DMs from whitelisted users (no need to share a server) |
| `respond_to_mentions` | `false` | Bot also replies in non-whitelisted channels when you `@`-mention it |
| `notify_on_idle` | `false` | Forward the noisy "Claude is waiting for your input" pings |
| `slash_command_guild_id` | `0` | Pin slash commands to one server for instant updates (else ~1h propagation) |
| `reply_quote_in_prompt` | `true` | Discord-replies prepend `[replying to: "..."]` so claude sees the context |
| `react_with_actions` | `true` | 🛑 / 💤 / 🔄 reactions trigger broker actions |

Edit any with `.\agentmux config set discord <key> <value>` then restart the bot.

## System tray + toast (no IM required)

Once `agentmux start` runs, an `agentmux-tray.exe` process attaches a
**coloured circle icon** to your Windows tray:

- **gray** — broker offline or no sessions
- **green** — sessions exist, all idle
- **yellow** — at least one session has an attached viewer
- **red** — at least one session waiting on tool approval / crashed

**Right-click** the icon for a per-session menu (Attach / Interrupt /
Hibernate / Restart / Kill), Open web viewer, Stop broker, Quit tray.

**Toasts** pop up automatically on three event kinds:

| Event | Toast looks like | Click does |
|---|---|---|
| `assistant_message` | `✅ [default] turn complete · "<answer preview>..."` | Spawns Windows Terminal with `claude-attach.exe --session default` |
| `notification` | `⚠️ [default] needs attention · <message>` | Opens that session |
| `tool_request` | `🔐 [default] approve Bash? · $ rm -rf /tmp/x` plus `[Allow]` `[Deny]` buttons | The buttons fire `agentmux://approve/<id>` / `agentmux://deny/<id>` deeplinks; the running tray catches them and POSTs the decision back to broker |

Toast action buttons + Discord button cards run **in parallel** for tool
approvals — whichever you click first wins, the other path is idempotent
(broker 404s the loser).

The tray registers the `agentmux://` URL scheme in `HKCU` on first launch
(no admin needed). Single-instance handshake via a named pipe makes
sure protocol-activation re-launches forward URLs to the running tray
rather than starting duplicate copies.

## Tool-use approval (PreToolUse)

Whenever claude wants to run a "risky" tool (e.g. `Bash` with `rm -rf` / `curl`, or `Write` / `Edit` outside the session's working directory), agentmux fans the request out to **both** Discord (button card) and the local tray (toast with action buttons). The hook waits up to 5 minutes; on timeout it returns `deny` and claude moves on.

Most turns trigger **zero** prompts — `Read` / `Glob` / `Grep` / `cargo` / `git status` / `ls` and 30-odd other dev verbs auto-allow. The classifier's logic lives in `crates/hook-pretool/src/main.rs` if you want to read or tweak the rules.

When the broker is unreachable, the hook fails *open* (allows the tool) so a busted Discord doesn't grind claude to a halt — the tradeoff is "approval surface degrades to no-op when the broker is down".

## Live progress narration (PostToolUse)

After every tool call, the `hook-posttool` hook posts a `tool_progress`
event to broker. Discord turns this into edit-in-place updates: the
`💭 working…` placeholder grows a running list of one-liners
(`✏️ edit src/x.rs`, `🖥 $ cargo test`, `🔎 grep "AuthMiddleware"`) so you
can see what claude's doing while the turn is still in flight, instead of
staring at an idle placeholder for minutes. When the turn completes, the
whole timeline is replaced with claude's actual answer.

Throttled at one Discord edit per 800 ms; history capped at the last 8
entries to keep mobile-Discord readable. The system tray doesn't render
this stream (would be too noisy as toast spam) — it's a Discord-only
feature for now.

## Editing config files

```powershell
.\agentmux config edit                    # opens config.toml in $EDITOR / VS Code / notepad
.\agentmux config edit discord            # discord.toml
.\agentmux config edit hooks              # ~\.claude\settings.json
.\agentmux config dir                     # opens %LOCALAPPDATA%\agentmux in Explorer
.\agentmux config check                   # validates all configs without restarting
```

`config check` is the friend you call after hand-editing — it parses the file, reports any TOML / JSON issues with line numbers, and confirms semantic invariants (e.g. `allowed_user_ids` non-empty).

## Attach from another machine (LAN mode)

The broker can serve `claude-attach` viewers from elsewhere on your local
network over HTTP/WebSocket, gated by a Bearer token. Loopback callers
(Discord bot, hooks, local `claude-attach`) **bypass** the token check, so
turning LAN mode on doesn't break anything that already works.

### 1. Configure the broker host

```powershell
# Generate a 32-byte token, write it to broker config, and print it.
# COPY THE PRINTED TOKEN — it's the only time it's shown plainly.
.\agentmux config token --set

# Bind the HTTP/WS listener to all interfaces (default is loopback only).
.\agentmux config set broker http_addr "0.0.0.0:8765"

# Restart so the new bind / token take effect.
.\agentmux stop
.\agentmux start

# Find the LAN IP to give the remote machine.
Get-NetIPAddress -AddressFamily IPv4 |
    Where-Object { $_.IPAddress -like "192.168.*" -or $_.IPAddress -like "10.*" } |
    Select-Object IPAddress, InterfaceAlias
```

Open the Windows firewall for port 8765 — restrict to your subnet, do
**not** open it to `0.0.0.0/0`:

```powershell
# Run this PS as Administrator. Replace 192.168.0.0/16 with your actual subnet,
# e.g. 192.168.1.0/24 or 10.0.0.0/24.
New-NetFirewallRule -DisplayName "agentmux broker (LAN)" `
    -Direction Inbound -Protocol TCP -LocalPort 8765 `
    -RemoteAddress 192.168.0.0/16 `
    -Action Allow
```

### 2. Configure the remote machine

Extract the same release zip there (no Rust, no broker, no hooks needed —
just `claude-attach.exe` and the wrapper). Then persist the token to a
User-scope environment variable:

```powershell
[Environment]::SetEnvironmentVariable("AGENT_ATTACH_TOKEN", "<paste-token>", "User")
```

⚠️ **Reopen the PowerShell window** before continuing — already-open
shells won't see the new variable. Confirm:

```powershell
$env:AGENT_ATTACH_TOKEN.Length    # should print the token's character count, not 0
```

### 3. Connect

```powershell
cd C:\Tools\agentmux                                       # wherever you extracted to
.\agentmux attach --broker http://192.168.X.Y:8765 --session default
```

Or call the binary directly without the wrapper:

```powershell
.\bin\claude-attach.exe --broker http://192.168.X.Y:8765 --session default

# Single-shot token via flag instead of env var:
.\bin\claude-attach.exe --broker http://192.168.X.Y:8765 --token "<token>" --session default
```

### Verifying connectivity (when something goes wrong)

```powershell
# 1. Can we reach the port at all?
Test-NetConnection -ComputerName 192.168.X.Y -Port 8765

# 2. Is the broker speaking HTTP and demanding auth?
curl.exe http://192.168.X.Y:8765/sessions
# expected: 401 Unauthorized — proves broker is up and token gating works

# 3. Does our token actually work?
curl.exe -H "Authorization: Bearer $env:AGENT_ATTACH_TOKEN" http://192.168.X.Y:8765/sessions
# expected: 200 with a JSON list of sessions
```

| Symptom | Cause |
|---|---|
| `connection refused` | broker still bound to 127.0.0.1, or not running at all |
| `connection timeout` | firewall closed / wrong subnet / wrong IP |
| `401 Unauthorized` from step 3 | wrong token, or env var didn't propagate (reopen PS) |
| works from `curl` but `agentmux attach` errors out | env var present in shell? `$env:AGENT_ATTACH_TOKEN.Length` |

### Recovering a lost token

The token is stored on the broker host in `broker.toml`. To read it back:

```powershell
# On the broker host
.\agentmux config show broker | Select-String attach_token
```

To rotate (invalidates the old one — every remote viewer needs the new one):

```powershell
.\agentmux config token --set
.\agentmux stop ; .\agentmux start
```

### Multiple viewers on the same session

Local pipe + LAN WS attaches are peer transports — broker doesn't
distinguish. So you can have:

- the broker host's own terminal attached locally (`.\agentmux attach default`)
- the remote machine attached over LAN (`.\agentmux attach --broker http://... --session default`)

…both watching the same TUI in real time. `.\agentmux status` on the
broker host reports `viewers=N`. Output is mirrored, input is merged in
arrival order, and resize coordinates to the smallest pane (so claude
never overflows the smaller window). Don't both type at once — claude
has one input field and concurrent keystrokes interleave.

### From a browser (no install)

Same broker, no client-side install. From any device that can reach the
broker host:

```
http://192.168.X.Y:8765/
```

The page is served from broker.exe itself (xterm.js + the fit addon
embedded via `include_bytes!`, no CDN), so it works on isolated LANs
with no internet access.

- **Loopback browsers** (`http://127.0.0.1:8765/`) skip the token prompt
  entirely — the auth middleware exempts loopback, same as for native
  tooling.
- **LAN browsers** see a token entry; paste the same `attach_token` you
  generated above. It persists to `localStorage` so refreshes don't
  re-prompt. WebSocket auth uses a `Sec-WebSocket-Protocol` subprotocol
  carrying the token (browsers cannot set the `Authorization` header
  on WebSockets), validated by the broker's middleware.
- **Auto-reconnect** with exponential backoff (1 s → 30 s cap) when
  the broker restarts. Scrollback survives the gap.
- **Touch devices** get a soft-key bar at the bottom with control keys
  (Esc / Tab / arrows / `^C` `^D` `^L` `^Z`), 28 ASCII punctuation
  buttons (`, . _ - / : ; ? ! ' " ( ) [ ] { } \ | = + * & < > # @ $` —
  iOS soft keyboards bury most of these, and several never reach xterm
  through the standard input pipeline), a **📋 paste modal** (a visible
  textarea you long-press-paste into then Send — works around iOS's
  refusal to long-press-paste into xterm's hidden helper textarea), and
  **⏫ ⇞ ⇟ ⏬ scroll controls** (xterm's touch-scroll on iOS is
  sluggish, so jump-to-top / page-up / page-down / jump-to-bottom
  buttons make scrollback navigation tractable). Tapping `^C` on the
  bar bypasses the Ctrl+C escalation tracker on purpose, so
  triple-tapping doesn't accidentally shut the broker down.

The browser viewer is a peer of `claude-attach` over the named pipe
or LAN WebSocket — same fan-out, same input merging, same resize
coordination. Multiple viewers (browser, native, IM) on the same
session all see the same TUI.

### Reverting to loopback-only

```powershell
.\agentmux config unset broker http_addr      # back to 127.0.0.1:8765
.\agentmux stop
.\agentmux start

# Optional: drop the firewall rule too.
Remove-NetFirewallRule -DisplayName "agentmux broker (LAN)"
```

The `attach_token` stays in `broker.toml` — harmless when loopback-only
since it's only checked on non-loopback requests. Re-enabling LAN later
needs nothing more than changing `http_addr` back.

## Take over a local claude conversation (`adopt` / `demote`)

The story: you're working in a regular Windows Terminal with `claude`,
have a useful conversation going, and need to leave the desk. You want
to keep that **same conversation** reachable from Discord later.

```powershell
# 1. (in the local terminal) get claude's session id, then exit
#    Inside claude: type /status — note the UUID, then /exit (or Ctrl+C)

# 2. bring it under broker (cwd defaults to your current shell cwd)
.\agentmux adopt --resume <claude-session-id>
#    e.g. .\agentmux adopt --resume 16d0b111-1959-4f87-9a74-9317a9302ffd

# 3. confirm
.\agentmux status                # new session shows up, state = idle

# 4. attach from anywhere
.\agentmux attach <name>         # local viewer
# or send a Discord message in the bound channel
# or open http://<broker>:8765/ from your phone
```

**Going the other way** (broker → local): hand a session back to a
local terminal session.

```powershell
.\agentmux demote default
# prints: cd "G:\path"; claude --resume <id>     ← copy the whole line
```

While **locally-owned**:

- Discord input is refused with a 💤 reaction + a one-time guidance reply (5-min window)
- Tray icon turns purple, the per-session menu offers **Re-adopt to broker**
- `/input`, `/interrupt`, `/restart`, `/hibernate` all return `409 locally_owned`
- The session record (cwd, channel bindings, claude_session_id) survives broker restart

To bring it back:

```powershell
# IMPORTANT: exit your local `claude --resume` first (Ctrl+C / /exit)
# otherwise two processes on the same session id will corrupt the transcript
.\agentmux adopt default
```

## When things go wrong

- **Discord bot doesn't reply.** First message after a hibernate takes ~3-5 s while claude resumes. If it never replies: `.\agentmux logs discord` and `.\agentmux logs broker` — usual culprits are the `MESSAGE CONTENT INTENT` toggle in the Discord developer portal, or hooks not installed (`.\agentmux hooks check`).
- **Bot replies but the message gets stuck in claude's TUI.** Was an issue with multi-line Discord messages and long input lines; broker now writes the text and `\r` separately with a 30 ms gap so paste-bursts submit cleanly. If you still see this on a fresh build, file a bug with the broker log.
- **`💭 working…` placeholder never finishes.** The bot crashed mid-turn. On its next start the orphan placeholder is auto-edited to `❌ this turn was interrupted by a bot restart`; the file `discord-pending.toml` under `%LOCALAPPDATA%\agentmux\` carries the recovery list.
- **Slash commands don't show up in Discord.** Global registration takes up to 1 hour to propagate. Pin them to your guild for instant updates: `.\agentmux config set discord slash_command_guild_id <your-guild-id>`, then restart the bot.
- **PreToolUse asks too often / not enough.** The classifier is hardcoded in `crates/hook-pretool/src/main.rs` (`AUTO_ALLOW_TOOLS`, `BASH_ALLOW_PREFIXES`, `BASH_ALWAYS_ASK`). Tweak and `cargo build --release -p hook-pretool`.
- **Broker won't start, says "already running".** A previous broker exited without cleaning up. Run `.\agentmux stop` then `.\agentmux start` — the start script self-heals stale PID files.
- **Need to debug the broker's stdio.** `.\agentmux start --foreground` runs it inline so panics and `tracing` lines stream to the current shell.
- **Remote attach hangs / 401s.** Check `.\agentmux config check broker` — `attach_token` must be set and `http_addr` must bind to a non-loopback interface (`0.0.0.0:8765`). Verify with `Get-NetTCPConnection -LocalPort 8765`.

## Going further

The [README](README.md) covers the HTTP control plane, configuration tables
for broker and Discord configs, the wire protocol between viewer and broker,
and how to add new IM platforms.

For contributors / future maintainers: the architecture's *why*, the protocol
wire formats, and an unimplemented P2P-attach proposal live in
[docs/PLAN.md](docs/PLAN.md). It's a Chinese-language design log — new users
don't need to read it.
