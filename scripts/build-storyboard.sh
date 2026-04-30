#!/usr/bin/env bash
# Linux/macOS counterpart of build-storyboard.ps1. Uses python3 (which
# ships with every modern Linux/macOS) for the base64 + string-replace
# step because shell substitution is awkward at the lengths involved
# (a 100 KB PNG is ~140 KB base64, which trips sed on some BSDs).

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TEMPLATE="$ROOT/docs/storyboard/template.svg"
RAW_DIR="${RAW_DIR:-$ROOT/docs/storyboard/raw}"
OUTPUT="${OUTPUT:-$ROOT/docs/storyboard.svg}"

if [[ ! -f "$TEMPLATE" ]]; then
    echo "template not found: $TEMPLATE" >&2
    exit 1
fi

if ! command -v python3 >/dev/null 2>&1; then
    echo "python3 is required but not on PATH" >&2
    exit 1
fi

echo "agentmux storyboard builder"
echo "  template: $TEMPLATE"
echo "  raw dir : $RAW_DIR"
echo "  output  : $OUTPUT"
echo

python3 - "$TEMPLATE" "$OUTPUT" "$RAW_DIR" <<'PY'
import base64, sys, pathlib
template_path, output_path, raw_dir = sys.argv[1:]
svg = pathlib.Path(template_path).read_text(encoding='utf-8')
raw = pathlib.Path(raw_dir)
for i in range(1, 5):
    png = raw / f'{i}.png'
    if not png.exists():
        sys.stderr.write(f'missing screenshot: {png}\n')
        sys.stderr.write('See docs/storyboard/README.md for what each slot needs.\n')
        sys.exit(1)
    data = png.read_bytes()
    b64 = base64.b64encode(data).decode('ascii')
    svg = svg.replace(f'REPLACE_ME_{i}', f'data:image/png;base64,{b64}')
    print(f'  + slot {i}: {png.name} ({len(data) / 1024:.1f} KB)')
if 'REPLACE_ME_' in svg:
    sys.stderr.write('unsubstituted REPLACE_ME_ placeholder remained in the SVG\n')
    sys.exit(1)
out = pathlib.Path(output_path)
out.write_text(svg, encoding='utf-8')
print(f'\nstoryboard built: {out} ({out.stat().st_size / 1024:.1f} KB)')
PY

echo
echo "Embed in README.md (top, after the badges):"
echo
echo '  <p align="center">'
echo '    <img src="docs/storyboard.svg" alt="agentmux: detach your terminal, approve tool calls from your phone, reattach later" width="900">'
echo '  </p>'
