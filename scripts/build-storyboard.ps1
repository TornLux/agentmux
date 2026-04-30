# Inline 4 PNG screenshots into docs/storyboard/template.svg as base64
# data URIs and write the result to docs/storyboard.svg, ready to embed
# in the project README via <img src="docs/storyboard.svg">.
#
# Why base64 inline: GitHub's SVG renderer in markdown sandboxes external
# image references through camo and applies sanitization that occasionally
# trips on cross-file <image href="..."> links. A self-contained SVG
# always renders. Cost is the ~33% base64 overhead, which is negligible
# for screenshots in the < 200 KB range.
#
# Inputs:
#   docs/storyboard/raw/{1,2,3,4}.png
# Output:
#   docs/storyboard.svg
#
# See docs/storyboard/README.md for what each slot should contain.

[CmdletBinding()]
param(
    [string]$RawDir,
    [string]$Output
)

$ErrorActionPreference = "Stop"

$root = Split-Path -Parent $PSScriptRoot
if (-not $RawDir) { $RawDir = Join-Path $root "docs\storyboard\raw" }
if (-not $Output) { $Output = Join-Path $root "docs\storyboard.svg" }
$template = Join-Path $root "docs\storyboard\template.svg"

if (-not (Test-Path $template)) { throw "template not found: $template" }

Write-Host "agentmux storyboard builder" -ForegroundColor Cyan
Write-Host "  template: $template"
Write-Host "  raw dir : $RawDir"
Write-Host "  output  : $Output"
Write-Host ""

$svg = Get-Content $template -Raw

for ($i = 1; $i -le 4; $i++) {
    $png = Join-Path $RawDir "$i.png"
    if (-not (Test-Path $png)) {
        throw @"
missing screenshot: $png

Each slot needs a PNG. See docs\storyboard\README.md for what to put
in each. Quick reminder:
  1.png — Windows Terminal with claude TUI mid-task
  2.png — visual cue for "I left" (lockscreen / closed laptop / stock)
  3.png — phone screenshot of Discord with tool_request card visible
  4.png — Terminal again after agentmux attach
"@
    }
    $bytes = [System.IO.File]::ReadAllBytes($png)
    $b64 = [Convert]::ToBase64String($bytes)
    $svg = $svg.Replace("REPLACE_ME_$i", "data:image/png;base64,$b64")
    $kb = [Math]::Round($bytes.Length / 1KB, 1)
    Write-Host ("  + slot {0}: {1}.png ({2} KB)" -f $i, $i, $kb) -ForegroundColor DarkGray
}

if ($svg -match "REPLACE_ME_") {
    throw "an unsubstituted REPLACE_ME_ placeholder remained in the SVG"
}

# UTF-8 *without* BOM — GitHub's SVG renderer handles either, but no BOM
# is the cleaner default and matches what most editors produce.
[System.IO.File]::WriteAllText($Output, $svg, [System.Text.UTF8Encoding]::new($false))
$size = (Get-Item $Output).Length

Write-Host ""
Write-Host ("storyboard built: {0} ({1:N1} KB)" -f $Output, ($size / 1KB)) -ForegroundColor Green
Write-Host ""
Write-Host "Embed in README.md (top, after the badges):" -ForegroundColor Cyan
Write-Host ""
Write-Host '  <p align="center">' -ForegroundColor Yellow
Write-Host '    <img src="docs/storyboard.svg" alt="agentmux: detach your terminal, approve tool calls from your phone, reattach later" width="900">' -ForegroundColor Yellow
Write-Host '  </p>' -ForegroundColor Yellow
