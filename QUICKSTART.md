# agentmux — Quickstart

## What you need

- **Windows 10/11** — agentmux uses ConPTY and Win32 named pipes; not portable
- **[Claude Code CLI](https://docs.anthropic.com/en/docs/claude-code)** on `PATH` — install with `npm install -g @anthropic-ai/claude-code`

## Install

Download `agentmux-vX.Y.Z-windows-x86_64.zip` from the [releases page](https://github.com/anthropics/agentmux/releases) and extract it anywhere — e.g. `C:\Tools\agentmux\`.

That folder is self-contained; no installer, no PATH changes, no admin rights needed.

## Set up

In a PowerShell window opened in the extracted folder:

```powershell
.\agentmux init
```

The wizard walks you through:

1. **Prerequisite check** — confirms binaries are present and `claude` is on PATH
2. **Hooks** — wires Claude Code's Stop / Notification hooks into `~\.claude\settings.json` so agentmux can see when claude finishes a turn (idempotent; skip if you've installed before)
3. **Broker config** — writes `%LOCALAPPDATA%\agentmux\config.toml` if missing (all defaults; you'll rarely need to edit)
4. **Discord IM** *(optional)* — prompts for bot token, channel ID, and your user ID. The token is verified against Discord before being saved to a User-scope environment variable
5. **Start broker** — launches the daemon as a background process

After that, you have a running agentmux. Type:

```powershell
.\agentmux attach            # enter claude's TUI
.\agentmux status            # see what's running
.\agentmux help              # full command list
```

## Daily ops cheat sheet

```powershell
.\agentmux start             # broker + Discord bot (if configured)
.\agentmux stop              # both
.\agentmux status            # one-line health summary
.\agentmux logs broker       # tail today's broker log
.\agentmux logs discord      # tail Discord adapter log
.\agentmux logs events       # tail events.jsonl (audit trail)

.\agentmux attach            # picker menu of sessions
.\agentmux attach default    # attach directly
.\agentmux attach --new foo  # create + attach a new session
```

Inside `claude-attach`:

| Keys | Action |
|---|---|
| Ctrl+Q or Ctrl+] | Detach (broker keeps running) |
| Ctrl+C ×1 | Interrupt claude's current turn |
| Ctrl+C ×2 within 1.5 s | Restart claude in this session (history preserved) |
| Ctrl+C ×3 within 1.5 s | ⚠️ Shut down the entire broker |

## Updating Discord settings later

```powershell
.\agentmux discord setup                  # add channels/users interactively
.\agentmux discord token                  # rotate the bot token (verified live)
.\agentmux discord users add <id>         # whitelist a user
.\agentmux discord channels remove <id>   # un-whitelist a channel
```

## Editing config files

```powershell
.\agentmux config edit                    # opens config.toml in $EDITOR / VS Code / notepad
.\agentmux config edit discord            # discord.toml
.\agentmux config edit hooks              # ~\.claude\settings.json
.\agentmux config dir                     # opens %LOCALAPPDATA%\agentmux in Explorer
.\agentmux config check                   # validates all configs without restarting
```

`config check` is the friend you call after hand-editing — it parses the file, reports any TOML / JSON issues with line numbers, and confirms semantic invariants (e.g. `allowed_user_ids` non-empty).

## When things go wrong

- **Discord bot doesn't reply.** First message after a hibernate takes ~3-5 s while claude resumes. If it never replies: `.\agentmux logs discord` and `.\agentmux logs broker` — usually the issue is the `MESSAGE CONTENT INTENT` toggle in the Discord developer portal, or hooks not installed (`.\agentmux hooks check`).
- **Broker won't start, says "already running".** A previous broker exited without cleaning up. Run `.\agentmux stop` then `.\agentmux start` — the start script self-heals stale PID files.
- **Need to debug the broker's stdio.** `.\agentmux start --foreground` runs it inline so panics and `tracing` lines stream to the current shell.

## Going further

The full architecture and design rationale live in [PLAN.md](PLAN.md). The
[README](README.md) covers the HTTP control plane, the wire protocol between
viewer and broker, and how to add new IM platforms.
