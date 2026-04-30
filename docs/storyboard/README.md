# Storyboard assets

`docs/storyboard.svg` is the 4-panel hero image embedded near the top
of the project README. The story it tells:

> **Sessions outlive your terminal. And your desk.**
>
> 1. At your desk → 2. Walk away → 3. Approve from phone → 4. Back at desk

The version that ships in this repo is **fully illustrated** —
no screenshots required. Every element (terminal mockups, closed
laptop, phone with Discord card, finger pointing at Allow) is drawn
with SVG primitives. This means:

- The storyboard never goes stale when claude's TUI changes
- Same image renders identically on Windows, Linux, macOS readers
- No risk of leaking private channel names, usernames, paths
- Total file size ~14 KB

If you want to swap any panel for a real screenshot — say, after
you've polished a demo session you'd like to show off — there's an
**advanced path** below.

## Editing the illustrated storyboard

`docs/storyboard.svg` is hand-written SVG. Edit it directly to tweak
text, colors, layout. The structure is four `<g transform="translate(x, y)">`
blocks (one per panel) plus a top tagline and three connecting arrows.
Most changes you'll want — captions, the demo prompt text, accent
colors — are obvious from the source.

After editing, drop the file into a browser to preview. GitHub
renders the same file inline in the README via:

```markdown
<p align="center">
  <img src="docs/storyboard.svg" alt="agentmux: detach your terminal, approve tool calls from your phone, reattach later" width="900">
</p>
```

## Advanced path: replace panels with real screenshots

If you'd rather show genuine screenshots in any of the four panels,
this directory carries a screenshot-driven pipeline:

- `template.svg` — same layout as the default storyboard, but
  with `<image href="REPLACE_ME_N">` placeholders instead of inline
  illustrations.
- `../scripts/build-storyboard.ps1` (Windows) and
  `../scripts/build-storyboard.sh` (Unix) — scripts that read four
  PNGs, base64-encode them, and substitute them into `template.svg`
  to produce a self-contained `docs/storyboard.svg`.

To use the screenshot pipeline:

1. Create `docs/storyboard/raw/`.
2. Save four PNGs there as `1.png` through `4.png`. Recommended
   contents:

   | File | Slot | Capture |
   |---|---|---|
   | `raw/1.png` | At your desk | Windows Terminal showing claude TUI mid-conversation; portrait crop. |
   | `raw/2.png` | Walk away | Closed laptop / lockscreen / empty chair. Stock photo from unsplash.com works. |
   | `raw/3.png` | Approve from phone | Phone screenshot of Discord with a `tool_request` card. **Redact** private channel names, usernames, unrelated messages. |
   | `raw/4.png` | Back at desk | Terminal again after `agentmux attach`, showing the tool call from slot 3 has now executed. |

3. Run the build script:

   ```powershell
   .\scripts\build-storyboard.ps1
   ```

   ```bash
   bash scripts/build-storyboard.sh
   ```

   The script overwrites `docs/storyboard.svg` with one that has
   your screenshots base64-inlined. The README embed line stays
   the same.

### Tips for a clean screenshot run

- **Use a fresh demo session**, not your live work:
  `.\agentmux new demo -Cwd C:\demo-empty -Persist`. Pick a prompt
  that triggers exactly one tool approval (e.g.
  `"Run: curl -s https://httpbin.org/get | head -20, then explain"`).
- **Take slots 1, 2, 3, 4 in order, no edits in between.** Slot 4
  must be the same session after the tool call from slot 3
  completed.
- **Keep file sizes modest** — each PNG under ~200 KB keeps the
  final SVG under ~1 MB.

## Going back to the illustrated default

`git checkout docs/storyboard.svg` restores the illustration version
from the committed history. The screenshot pipeline doesn't fight
the default — it's a one-shot generator that produces a different
artifact at the same path.
