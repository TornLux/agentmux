# vendored web assets

Third-party JS/CSS baked into `broker.exe` via `include_bytes!` so the
web viewer works without external CDN access (offline / isolated LANs).

| File | Source | Version | License |
|---|---|---|---|
| `xterm.min.js` | https://www.npmjs.com/package/@xterm/xterm | 5.5.0 | MIT |
| `xterm.min.css` | (same package) | 5.5.0 | MIT |
| `addon-fit.min.js` | https://www.npmjs.com/package/@xterm/addon-fit | 0.10.0 | MIT |

License texts for both packages live at <https://github.com/xtermjs/xterm.js/blob/master/LICENSE>.

## Updating

```powershell
$dst = "crates\broker\web\vendor"
@(
    @{ Url = "https://cdn.jsdelivr.net/npm/@xterm/xterm@5.5.0/lib/xterm.min.js";       Name = "xterm.min.js" }
    @{ Url = "https://cdn.jsdelivr.net/npm/@xterm/xterm@5.5.0/css/xterm.min.css";      Name = "xterm.min.css" }
    @{ Url = "https://cdn.jsdelivr.net/npm/@xterm/addon-fit@0.10.0/lib/addon-fit.min.js"; Name = "addon-fit.min.js" }
) | ForEach-Object { Invoke-WebRequest $_.Url -OutFile (Join-Path $dst $_.Name) -UseBasicParsing }
```

Bump the versions in the URLs (and this table) when upgrading. Nothing
else needs changing — the broker `include_bytes!` paths and HTML refs
are version-agnostic.
