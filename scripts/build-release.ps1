# Build a release zip ready for end users to download + extract.
#
# Layout produced:
#   agentmux-vX.Y.Z-windows-x86_64/
#     bin/{broker,claude-attach,hook-stop,hook-notification,hook-pretool,
#          hook-posttool,platform-discord,agentmux-tray,agentmux-cli}.exe
#     scripts/        (all .ps1 + terminal-profile.json)
#     agentmux.ps1
#     README.md
#     QUICKSTART.md
#     PLAN.md
#
# Then zips it to ./dist/agentmux-vX.Y.Z-windows-x86_64.zip.
#
# Usage:
#   .\scripts\build-release.ps1                 # builds + zips
#   .\scripts\build-release.ps1 -SkipBuild      # reuses existing target/release/
#   .\scripts\build-release.ps1 -OutDir custom  # zip lands in custom\

[CmdletBinding()]
param(
    [switch]$SkipBuild,
    [string]$OutDir
)

$ErrorActionPreference = "Stop"

$root = Split-Path -Parent $PSScriptRoot
Push-Location $root

try {
    # --- read workspace version from root Cargo.toml ---------------------
    $cargoToml = Get-Content -LiteralPath (Join-Path $root "Cargo.toml") -Raw
    $match = [regex]::Match($cargoToml, '(?ms)^\[workspace\.package\].*?^version\s*=\s*"([^"]+)"')
    if (-not $match.Success) {
        throw "could not read [workspace.package].version from Cargo.toml"
    }
    $version = $match.Groups[1].Value
    $tag = "v$version"
    $platform = "windows-x86_64"
    $stem = "agentmux-$tag-$platform"

    Write-Host "agentmux release builder" -ForegroundColor Cyan
    Write-Host "  version:  $version"
    Write-Host "  artifact: $stem.zip"
    Write-Host ""

    # --- build -----------------------------------------------------------
    if (-not $SkipBuild) {
        Write-Host "[1/4] cargo build --release..." -ForegroundColor Cyan
        $cargo = Get-Command cargo -ErrorAction SilentlyContinue
        if (-not $cargo) {
            $homeCargo = Join-Path $env:USERPROFILE ".cargo\bin\cargo.exe"
            if (Test-Path $homeCargo) {
                $cargo = $homeCargo
            } else {
                throw "cargo not on PATH. Install Rust from https://rustup.rs/"
            }
        }
        & $cargo build --release
        if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }
    } else {
        Write-Host "[1/4] cargo build --release (skipped)" -ForegroundColor DarkGray
    }

    # --- stage ----------------------------------------------------------
    Write-Host "[2/4] staging tree..." -ForegroundColor Cyan
    $staging = Join-Path $root ".release-staging"
    if (Test-Path $staging) { Remove-Item -Recurse -Force $staging }
    $stageRoot = Join-Path $staging $stem
    New-Item -ItemType Directory -Path $stageRoot | Out-Null

    # bin/
    $binDir = Join-Path $stageRoot "bin"
    New-Item -ItemType Directory -Path $binDir | Out-Null
    $binaries = @(
        "broker.exe",
        "claude-attach.exe",
        "hook-stop.exe",
        "hook-notification.exe",
        "hook-pretool.exe",
        "hook-posttool.exe",
        "platform-discord.exe",
        "agentmux-tray.exe",
        "agentmux-cli.exe"
    )
    foreach ($b in $binaries) {
        $src = Join-Path $root "target\release\$b"
        if (-not (Test-Path $src)) {
            throw "missing build artifact: $src"
        }
        Copy-Item $src $binDir
        Write-Host "  + bin/$b"
    }

    # scripts/
    $stageScripts = Join-Path $stageRoot "scripts"
    New-Item -ItemType Directory -Path $stageScripts | Out-Null
    $scriptFiles = Get-ChildItem (Join-Path $root "scripts") -File |
        Where-Object { $_.Name -notin @("build-release.ps1") }
    foreach ($f in $scriptFiles) {
        Copy-Item $f.FullName $stageScripts
        Write-Host "  + scripts/$($f.Name)"
    }

    # top-level files
    foreach ($f in @("agentmux.ps1", "README.md", "QUICKSTART.md", "PLAN.md", "LICENSE")) {
        $src = Join-Path $root $f
        if (Test-Path $src) {
            Copy-Item $src $stageRoot
            Write-Host "  + $f"
        } else {
            Write-Host "  - $f (missing — skipped)" -ForegroundColor DarkGray
        }
    }

    # --- zip ------------------------------------------------------------
    Write-Host "[3/4] zipping..." -ForegroundColor Cyan
    $dist = if ($OutDir) { Resolve-Path -LiteralPath $OutDir -ErrorAction SilentlyContinue } else { Join-Path $root "dist" }
    if (-not $dist) { $dist = $OutDir }   # OutDir may not exist yet
    if (-not (Test-Path $dist)) { New-Item -ItemType Directory -Path $dist | Out-Null }
    $zip = Join-Path $dist "$stem.zip"
    if (Test-Path $zip) { Remove-Item $zip }
    # Compress the SINGLE folder so the zip extracts to agentmux-vX.Y.Z-...
    Compress-Archive -Path $stageRoot -DestinationPath $zip -CompressionLevel Optimal

    # --- cleanup --------------------------------------------------------
    Write-Host "[4/4] cleanup..." -ForegroundColor Cyan
    Remove-Item -Recurse -Force $staging

    $size = (Get-Item $zip).Length
    Write-Host ""
    Write-Host "release built:" -ForegroundColor Green
    Write-Host "  $zip"
    Write-Host ("  {0:N1} MB" -f ($size / 1MB))
} finally {
    Pop-Location
}
