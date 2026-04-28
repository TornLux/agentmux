# Write a documented template discord.toml at
# %LOCALAPPDATA%\agentmux\discord.toml. The bot token is intentionally
# NOT a field here — store it in an env var (default DISCORD_BOT_TOKEN)
# so it never lands in this file or its backups.
#
# Usage:
#   .\init-discord-config.ps1            # create if missing, refuse if exists
#   .\init-discord-config.ps1 -Force     # overwrite (existing file is backed up to .bak)
#   .\init-discord-config.ps1 -Edit      # also open the file in notepad afterwards

[CmdletBinding()]
param(
    [switch]$Force,
    [switch]$Edit
)

$ErrorActionPreference = "Stop"

$dir = Join-Path $env:LOCALAPPDATA "agentmux"
$cfg = Join-Path $dir "discord.toml"

New-Item -ItemType Directory -Path $dir -Force | Out-Null

if ((Test-Path -LiteralPath $cfg) -and -not $Force) {
    Write-Warning "discord.toml already exists at: $cfg"
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
# agentmux platform-discord config.
# Loaded by platform-discord.exe at startup.
#
# The bot token does NOT live here. Set it as an env var:
#   [Environment]::SetEnvironmentVariable("DISCORD_BOT_TOKEN", "<token>", "User")
# then reopen your shell.

# Env var name to read the bot token from. Change if you prefer
# another name; the value itself stays out of this file.
token_env = "DISCORD_BOT_TOKEN"

# Where the broker is reachable. Defaults match a local broker on
# 127.0.0.1:8765 with WS at /ws on the same listener.
broker_http_url = "http://127.0.0.1:8765"
broker_ws_url   = "ws://127.0.0.1:8765/ws"

# Discord channel IDs the bot listens in (server channels only).
# Empty = listen in any server channel the bot can see.
# DMs are governed by `allow_dm` below, NOT by this list.
# Right-click a channel with Developer Mode on -> Copy ID.
channel_ids = [
    # 123456789012345678,
]

# Discord user IDs whose messages the bot will forward. MUST be
# non-empty — the bot refuses to start otherwise. Right-click your
# avatar → Copy ID.
allowed_user_ids = [
    # 123456789012345678,
]

# Session name the bot is initially bound to. Switch at runtime with
# `!attach <name>`. Bot restart resets the binding to this value.
default_session = "default"

# Discord caps a single message at 2000 chars; the bot splits at this
# threshold to leave a small margin for decorators (`**[name]**` etc).
max_message_chars = 1900

# Accept messages in 1:1 DM channels with whitelisted users? When false
# (default) the bot only reads from server channels. Flip to true for
# solo / mobile use without a guild.
allow_dm = false

# Forward Claude Code's idle "waiting for your input" pings? Off by
# default (they fire ~60s after every reply and most users find them
# noisy). Permission prompts and other Notification messages always
# pass through regardless.
notify_on_idle = false

# Accept messages in non-whitelisted server channels when the bot is
# @mentioned? The mention is stripped from the prompt before forwarding.
# Useful for "let bot reach me anywhere" without listing every channel.
respond_to_mentions = false

# Optional guild id for slash-command registration. When set, /commands
# register as guild-scoped and update INSTANTLY. Leave 0 for global
# commands (work everywhere the bot is, but propagation can take ~1h
# after the bot first comes online or after definitions change).
slash_command_guild_id = 0

# When you Reply (Discord UI) to an earlier message, prepend a
# `[replying to: "..."]` header to the prompt forwarded to claude.
# Helpful when the referenced message is from someone else or from a
# different channel and claude wouldn't otherwise have it in context.
# Reply-thread session routing is independent of this flag.
reply_quote_in_prompt = true
'@

$utf8NoBom = New-Object System.Text.UTF8Encoding($false)
[System.IO.File]::WriteAllText($cfg, $content, $utf8NoBom)
Write-Host "wrote: $cfg"
Write-Host ""
Write-Host "next steps:"
Write-Host "  1. edit $cfg and fill in channel_ids + allowed_user_ids"
Write-Host "  2. set the bot token env var:"
Write-Host '     [Environment]::SetEnvironmentVariable("DISCORD_BOT_TOKEN", "<token>", "User")'
Write-Host "  3. reopen your shell so the env var is visible"
Write-Host "  4. .\start-discord.ps1"

if ($Edit) {
    Start-Process notepad.exe $cfg
}
