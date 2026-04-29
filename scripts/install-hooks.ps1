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
#   * Recognises our hook entries by exe-path equality (slash-normalised,
#     case-insensitive) so we don't duplicate, and we migrate legacy
#     backslash-shaped entries to the canonical forward-slash form
#     (Claude Code's bash mangles unquoted `\` in commands).
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

if (-not $hookStop -or -not $hookNotification -or -not $hookPreTool) {
    $msg = @"
Hook binaries not found. Looked in:
  $root\bin\               (release zip layout)
  $root\target\release\    (cargo build layout)

If you extracted from a release zip, re-extract — the zip should contain a bin\ folder
with hook-stop.exe / hook-notification.exe / hook-pretool.exe.
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

# True iff any group anywhere (incl. nested arrays from older bugs) has a
# hook command exactly equal to $cmd (byte-for-byte).
function Test-ExactCommand {
    param($eventArr, $cmd)
    foreach ($item in @($eventArr)) {
        if ($null -eq $item) { continue }
        if ($item -is [System.Collections.IList] -and -not ($item -is [string])) {
            if (Test-ExactCommand $item $cmd) { return $true }
            continue
        }
        if (-not ($item -is [System.Collections.IDictionary])) { continue }
        $hList = $item["hooks"]
        if ($null -eq $hList) { continue }
        foreach ($entry in @($hList)) {
            if ($null -eq $entry) { continue }
            if (($entry -is [System.Collections.IDictionary]) `
                -and ($entry["type"] -eq "command") `
                -and ($entry["command"] -eq $cmd)) {
                return $true
            }
        }
    }
    return $false
}

# True iff any entry's command resolves to the same path (slash- and
# case-normalised) — used to detect entries we should migrate.
function Test-HasCommand {
    param($eventArr, $cmd)
    foreach ($item in @($eventArr)) {
        if ($null -eq $item) { continue }
        if ($item -is [System.Collections.IList] -and -not ($item -is [string])) {
            if (Test-HasCommand $item $cmd) { return $true }
            continue
        }
        if (-not ($item -is [System.Collections.IDictionary])) { continue }
        $hList = $item["hooks"]
        if ($null -eq $hList) { continue }
        foreach ($entry in @($hList)) {
            if ($null -eq $entry) { continue }
            if (($entry -is [System.Collections.IDictionary]) `
                -and ($entry["type"] -eq "command") `
                -and (Test-PathEqual $entry["command"] $cmd)) {
                return $true
            }
        }
    }
    return $false
}

# Returns a flat Object[] of groups with our entries removed (matching
# by Test-PathEqual). Flattens any nested-array shapes from prior bugs.
function Remove-OurCommand {
    param($eventArr, $cmd)
    $kept = @()
    foreach ($item in @($eventArr)) {
        if ($null -eq $item) { continue }
        if ($item -is [System.Collections.IList] -and -not ($item -is [string])) {
            $sub = Remove-OurCommand $item $cmd
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
                      -and (Test-PathEqual $entry["command"] $cmd)
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
    @{ Event = "Stop";         Exe = $hookStop }
    @{ Event = "Notification"; Exe = $hookNotification }
    @{ Event = "PreToolUse";   Exe = $hookPreTool }
)

$changed = $false
foreach ($p in $pairs) {
    $evt = $p.Event
    $exe = $p.Exe

    $current = @()
    if ($hooks.Contains($evt) -and $null -ne $hooks[$evt]) {
        $current = @($hooks[$evt])
    }

    if ($Uninstall) {
        if (Test-HasCommand $current $exe) {
            $cleaned = Remove-OurCommand $current $exe
            if ($cleaned.Count -eq 0) {
                $hooks.Remove($evt)
            } else {
                $hooks[$evt] = $cleaned
            }
            Write-Host "removed: $evt -> $exe"
            $changed = $true
        }
    } else {
        if (Test-ExactCommand $current $exe) {
            Write-Host "already installed: $evt -> $exe"
        } elseif (Test-HasCommand $current $exe) {
            $cleaned = Remove-OurCommand $current $exe
            $newGroup = [ordered]@{
                hooks = @([ordered]@{ type = "command"; command = $exe })
            }
            $hooks[$evt] = $cleaned + ,$newGroup
            Write-Host "migrated to canonical path: $evt -> $exe"
            $changed = $true
        } else {
            $newGroup = [ordered]@{
                hooks = @([ordered]@{ type = "command"; command = $exe })
            }
            $hooks[$evt] = $current + ,$newGroup
            Write-Host "installed: $evt -> $exe"
            $changed = $true
        }
    }
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
