# Idempotent merger for Phase 5 hooks into Claude Code's user-global
# settings.json. Run once and you're done — claude in *any* working
# directory will fire hook-stop.exe / hook-notification.exe.
#
# Usage:
#   .\install-hooks.ps1              # install / refresh
#   .\install-hooks.ps1 -Uninstall   # remove our entries
#   .\install-hooks.ps1 -SettingsPath C:\custom\settings.json
#
# Behaviour:
#   * Backs up settings.json to settings.json.bak before touching it.
#   * Recognises our hook entries by **basename** (`hook-stop.exe` /
#     `hook-notification.exe` / `hook-pretool.exe` — names unique to
#     agentmux), not by full path. Reinstalling from a different folder
#     therefore dedups + repoints the single remaining entry at the
#     current build's exe rather than appending a second one. Forward
#     slashes always (Claude Code's bash mangles unquoted `\` in commands).
#   * Tolerant of accidentally-nested arrays from prior buggy runs.
#
# PowerShell array gotcha note: functions that build arrays return them
# via `return ,$arr` so callers receive the array intact. Callers must
# NOT wrap with `@(func)` — that would re-wrap the result.

[CmdletBinding()]
param(
    [string]$SettingsPath = (Join-Path $env:USERPROFILE ".claude\settings.json"),
    [switch]$Uninstall
)

$ErrorActionPreference = "Stop"

$root = Split-Path -Parent $PSScriptRoot

# Prefer bin/ (release-zip layout); fall back to target/release/ (cargo
# builds). Two layouts so the same script works for both release-zip
# users (no Rust toolchain) and from-source developers.
function Find-HookExe([string]$name) {
    $candidates = @(
        (Join-Path $root "bin\$name"),
        (Join-Path $root "target\release\$name")
    )
    foreach ($c in $candidates) {
        if (Test-Path -LiteralPath $c) { return $c }
    }
    return $null
}

$hookStop         = Find-HookExe "hook-stop.exe"
$hookNotification = Find-HookExe "hook-notification.exe"
$hookPreTool      = Find-HookExe "hook-pretool.exe"
$hookPostTool     = Find-HookExe "hook-posttool.exe"

if (-not $hookStop -or -not $hookNotification -or -not $hookPreTool -or -not $hookPostTool) {
    $msg = @"
Hook binaries not found. Looked in:
  $root\bin\               (release zip layout)
  $root\target\release\    (cargo build layout)

If you extracted from a release zip, re-extract — the zip should contain a bin\ folder
with hook-stop.exe / hook-notification.exe / hook-pretool.exe / hook-posttool.exe.
If you cloned from source, build with: cargo build --release
"@
    throw $msg
}
# Forward slashes — Claude Code on Windows runs hook commands via
# /usr/bin/bash, which silently eats unquoted backslashes. Forward
# slashes in exe paths work the same as backslashes for Windows process
# launching but survive bash unscathed.
$hookStop         = ((Resolve-Path -LiteralPath $hookStop).Path        -replace '\\', '/')
$hookNotification = ((Resolve-Path -LiteralPath $hookNotification).Path -replace '\\', '/')
$hookPreTool      = ((Resolve-Path -LiteralPath $hookPreTool).Path     -replace '\\', '/')
$hookPostTool     = ((Resolve-Path -LiteralPath $hookPostTool).Path    -replace '\\', '/')

function ConvertTo-OrderedHashtable {
    param($obj)
    if ($null -eq $obj) { return $null }
    if ($obj -is [System.Management.Automation.PSCustomObject]) {
        $h = [ordered]@{}
        foreach ($p in $obj.PSObject.Properties) {
            $h[$p.Name] = ConvertTo-OrderedHashtable $p.Value
        }
        return $h
    }
    if ($obj -is [System.Collections.IList] -and -not ($obj -is [string])) {
        $arr = @()
        foreach ($item in $obj) { $arr += ,(ConvertTo-OrderedHashtable $item) }
        return ,$arr
    }
    return $obj
}

function Test-PathEqual {
    param([string]$a, [string]$b)
    if ($null -eq $a -or $null -eq $b) { return $false }
    return ($a -replace '\\', '/').ToLowerInvariant() -eq ($b -replace '\\', '/').ToLowerInvariant()
}

# Extracts the lowercased file basename from a hook command string.
# A hook command is normally just an exe path; if it grows arguments
# in future, only the first whitespace-separated token is examined,
# which matches how shells resolve the program.
function Get-CommandBasename {
    param([string]$cmd)
    if ([string]::IsNullOrWhiteSpace($cmd)) { return "" }
    $first = ($cmd -split '\s', 2)[0]
    $norm  = $first -replace '\\', '/'
    $parts = $norm -split '/'
    return $parts[$parts.Count - 1].ToLowerInvariant()
}

function Test-BasenameEqual {
    param([string]$cmd, [string]$basename)
    return (Get-CommandBasename $cmd) -eq $basename.ToLowerInvariant()
}

# Walks the (possibly nested) event array and emits every hook command
# whose basename matches $basename, one per pipeline item. Caller wraps
# in @(...) to collect into an array. Pipeline-emission avoids the
# `$arr += ,$x` PowerShell quirk where the empty-array seed gets
# unrolled and subsequent appends silently coerce to string concat.
function Get-OurCommands {
    param($eventArr, $basename)
    foreach ($item in @($eventArr)) {
        if ($null -eq $item) { continue }
        if ($item -is [System.Collections.IList] -and -not ($item -is [string])) {
            Get-OurCommands $item $basename
            continue
        }
        if (-not ($item -is [System.Collections.IDictionary])) { continue }
        $hList = $item["hooks"]
        if ($null -eq $hList) { continue }
        foreach ($entry in @($hList)) {
            if ($null -eq $entry) { continue }
            if (($entry -is [System.Collections.IDictionary]) `
                -and ($entry["type"] -eq "command") `
                -and (Test-BasenameEqual $entry["command"] $basename)) {
                $entry["command"]
            }
        }
    }
}

# Returns a flat Object[] of groups with all entries whose command
# basename matches $basename removed. Flattens any nested-array shapes
# from prior buggy runs as a side effect.
function Remove-ByBasename {
    param($eventArr, $basename)
    $kept = @()
    foreach ($item in @($eventArr)) {
        if ($null -eq $item) { continue }
        if ($item -is [System.Collections.IList] -and -not ($item -is [string])) {
            $sub = Remove-ByBasename $item $basename
            foreach ($s in $sub) { $kept += ,$s }
            continue
        }
        if (-not ($item -is [System.Collections.IDictionary])) {
            $kept += ,$item
            continue
        }
        $hList = $item["hooks"]
        if ($null -eq $hList) { $kept += ,$item; continue }
        $newList = @()
        foreach ($entry in @($hList)) {
            $isOurs = ($entry -is [System.Collections.IDictionary]) `
                      -and ($entry["type"] -eq "command") `
                      -and (Test-BasenameEqual $entry["command"] $basename)
            if (-not $isOurs) { $newList += ,$entry }
        }
        if ($newList.Count -gt 0) {
            $item["hooks"] = $newList
            $kept += ,$item
        }
    }
    return ,$kept
}

# --- Load current settings.json ---
$settings = [ordered]@{}
if (Test-Path -LiteralPath $SettingsPath) {
    $raw = Get-Content -LiteralPath $SettingsPath -Raw
    if ($raw.Trim().Length -gt 0) {
        $parsed = $raw | ConvertFrom-Json
        $settings = ConvertTo-OrderedHashtable $parsed
        if ($settings -isnot [System.Collections.IDictionary]) {
            throw "settings.json top-level is not a JSON object"
        }
    }
}

if (-not $settings.Contains("hooks")) { $settings["hooks"] = [ordered]@{} }
$hooks = $settings["hooks"]

$pairs = @(
    @{ Event = "Stop";         Exe = $hookStop;         Basename = "hook-stop.exe" }
    @{ Event = "Notification"; Exe = $hookNotification; Basename = "hook-notification.exe" }
    @{ Event = "PreToolUse";   Exe = $hookPreTool;      Basename = "hook-pretool.exe" }
    @{ Event = "PostToolUse";  Exe = $hookPostTool;     Basename = "hook-posttool.exe" }
)

$changed = $false
foreach ($p in $pairs) {
    $evt = $p.Event
    $exe = $p.Exe
    $bn  = $p.Basename

    $current = @()
    if ($hooks.Contains($evt) -and $null -ne $hooks[$evt]) {
        $current = @($hooks[$evt])
    }

    $ours = @(Get-OurCommands $current $bn)

    if ($Uninstall) {
        if ($ours.Count -gt 0) {
            $cleaned = Remove-ByBasename $current $bn
            if ($cleaned.Count -eq 0) {
                $hooks.Remove($evt)
            } else {
                $hooks[$evt] = $cleaned
            }
            Write-Host "removed: $evt ($($ours.Count) entr$( if ($ours.Count -eq 1) {'y'} else {'ies'} ))"
            $changed = $true
        }
        continue
    }

    # Install: enforce exactly one entry pointing at the current build's
    # exe. Sweep every basename match (regardless of which path it points
    # at), then add a fresh entry — that way reinstall from any folder
    # converges to a single, canonical entry.
    $alreadyCanonical = ($ours.Count -eq 1) -and (Test-PathEqual $ours[0] $exe)
    if ($alreadyCanonical) {
        Write-Host "already installed: $evt -> $exe"
        continue
    }

    $cleaned = Remove-ByBasename $current $bn
    $newGroup = [ordered]@{
        hooks = @([ordered]@{ type = "command"; command = $exe })
    }
    $hooks[$evt] = $cleaned + ,$newGroup

    if ($ours.Count -eq 0) {
        Write-Host "installed: $evt -> $exe"
    } elseif ($ours.Count -eq 1) {
        Write-Host "migrated:  $evt -> $exe"
        Write-Host "             (was: $($ours[0]))"
    } else {
        Write-Host "consolidated $($ours.Count) duplicate $evt entries -> $exe"
        foreach ($o in $ours) { Write-Host "             dropped: $o" }
    }
    $changed = $true
}

if ($Uninstall -and $hooks.Count -eq 0) {
    $settings.Remove("hooks")
    $changed = $true
}

if (-not $changed) {
    Write-Host "no changes."
    return
}

$settingsDir = Split-Path -Parent $SettingsPath
New-Item -ItemType Directory -Path $settingsDir -Force | Out-Null

if (Test-Path -LiteralPath $SettingsPath) {
    Copy-Item -LiteralPath $SettingsPath -Destination "$SettingsPath.bak" -Force
    Write-Host "backup: $SettingsPath.bak"
}

$json = $settings | ConvertTo-Json -Depth 32
$utf8NoBom = New-Object System.Text.UTF8Encoding($false)
[System.IO.File]::WriteAllText($SettingsPath, $json, $utf8NoBom)
Write-Host "wrote: $SettingsPath"
