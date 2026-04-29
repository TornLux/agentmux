//! Render `tool_progress` events into human-readable progress lines
//! for in-place placeholder edits.
//!
//! The placeholder posted when a Discord prompt is forwarded
//! (`💭 working…`) gets replaced as PostToolUse hooks fire, so the
//! user sees the live narrative of what claude is doing. When the
//! turn completes, `assistant_message` consumes the placeholder and
//! replaces the whole thing with the final answer.
//!
//! Goals:
//!   * Per-tool emoji + 1-line summary that conveys *what* without
//!     leaking pages of input/output to Discord.
//!   * Truncation/eliding of long paths and Bash commands so a giant
//!     `cargo test --workspace --all-features` doesn't blow the line.
//!   * Bounded total length: keep only the last `MAX_HISTORY_LINES`
//!     so a 50-tool turn doesn't grow unbounded.

use serde_json::Value;
use std::time::Duration;

/// Placeholder is edited at most this often per pending message —
/// quiet enough to stay well under Discord's 5/5s edit rate limit on
/// a single message even with bursts of fast tool calls (Glob+Read
/// chains during exploration), but visibly responsive.
pub const PROGRESS_EDIT_THROTTLE: Duration = Duration::from_millis(800);

/// History rolling window. Older entries are dropped before each edit
/// so the placeholder body stays short. Eight is comfortable on
/// mobile Discord without scrolling.
const MAX_HISTORY_LINES: usize = 8;

/// Cap path lengths at this many chars in progress lines — file
/// paths under a deep cwd quickly bloat past Discord's mobile width.
const MAX_PATH_CHARS: usize = 80;

/// Cap Bash command / search query / URL previews here.
const MAX_INLINE_CHARS: usize = 80;

/// Translate one PostToolUse event payload into a single progress
/// line ("✏️ editing `src/x.rs`", "🖥 `$ cargo test`", …). Unknown
/// tool names get a generic `🔧 <name>` line so future tools surface
/// without code changes.
pub fn render_tool_progress(tool_name: &str, tool_input: &Value) -> String {
    let s = |k: &str| {
        tool_input
            .get(k)
            .and_then(|v| v.as_str())
            .unwrap_or("?")
            .to_string()
    };

    match tool_name {
        "Read" => format!("📖 read `{}`", short_path(&s("file_path"))),
        "Edit" => format!("✏️ edit `{}`", short_path(&s("file_path"))),
        "MultiEdit" => format!("✏️ edit `{}`", short_path(&s("file_path"))),
        "Write" => format!("📝 write `{}`", short_path(&s("file_path"))),
        "NotebookEdit" => format!("📓 notebook `{}`", short_path(&s("notebook_path"))),
        "Glob" => format!("🔍 glob `{}`", truncate(&s("pattern"), MAX_INLINE_CHARS)),
        "Grep" => format!("🔎 grep `{}`", truncate(&s("pattern"), MAX_INLINE_CHARS)),
        "Bash" => format!("🖥 `$ {}`", truncate(&s("command"), MAX_INLINE_CHARS)),
        "BashOutput" => "📥 read shell output".to_string(),
        "KillShell" | "KillBash" => "🛑 killed shell".to_string(),
        "WebFetch" => format!("🌐 fetch `{}`", truncate(&s("url"), MAX_INLINE_CHARS)),
        "WebSearch" => format!(
            "🌐 search `{}`",
            truncate(&s("query"), MAX_INLINE_CHARS)
        ),
        "TodoWrite" => "📋 updated todos".to_string(),
        "TodoRead" => "📋 read todos".to_string(),
        "Task" => {
            let st = tool_input
                .get("subagent_type")
                .and_then(|v| v.as_str())
                .unwrap_or("agent");
            format!("🤖 launched `{st}` subagent")
        }
        "ExitPlanMode" => "📐 exited plan mode".to_string(),
        // MCP tools come through as `mcp__<server>__<tool>`. We don't
        // have a registry of servers, so render server + bare tool name.
        n if n.starts_with("mcp__") => {
            let mut parts = n.splitn(3, "__");
            let _ = parts.next();
            let server = parts.next().unwrap_or("?");
            let tool = parts.next().unwrap_or("?");
            format!("🔌 mcp `{server}.{tool}`")
        }
        other => format!("🔧 {other}"),
    }
}

/// Build the placeholder body from the running history. The leading
/// `💭 *working…*` italic line keeps the visual continuity of the
/// original placeholder so users intuitively know "still processing".
pub fn render_placeholder(history: &[String]) -> String {
    let trimmed: &[String] = if history.len() > MAX_HISTORY_LINES {
        &history[history.len() - MAX_HISTORY_LINES..]
    } else {
        history
    };
    let mut out = String::from("💭 *working…*");
    for line in trimmed {
        out.push('\n');
        out.push_str("• ");
        out.push_str(line);
    }
    out
}

/// Trim a long path: keep the last two path components plus an
/// ellipsis prefix if anything was dropped. `src/foo/bar/baz/quux.rs`
/// becomes `…/baz/quux.rs` once the full string exceeds MAX_PATH_CHARS.
fn short_path(path: &str) -> String {
    if path.chars().count() <= MAX_PATH_CHARS {
        return normalise_slashes(path);
    }
    let normalised = normalise_slashes(path);
    let parts: Vec<&str> = normalised.split('/').collect();
    if parts.len() <= 2 {
        return truncate(&normalised, MAX_PATH_CHARS);
    }
    let tail = parts[parts.len() - 2..].join("/");
    let candidate = format!("…/{tail}");
    if candidate.chars().count() <= MAX_PATH_CHARS {
        candidate
    } else {
        truncate(&candidate, MAX_PATH_CHARS)
    }
}

fn normalise_slashes(s: &str) -> String {
    s.replace('\\', "/")
}

/// Char-aware truncation that appends `…` when content was dropped.
/// Operates on chars (not bytes) so multi-byte glyphs like CJK don't
/// get sliced mid-character.
fn truncate(s: &str, max_chars: usize) -> String {
    let count = s.chars().count();
    if count <= max_chars {
        return s.to_string();
    }
    let keep = max_chars.saturating_sub(1);
    let mut out: String = s.chars().take(keep).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn renders_known_tools() {
        let line = render_tool_progress("Edit", &json!({"file_path": "src/x.rs"}));
        assert!(line.contains("edit"));
        assert!(line.contains("src/x.rs"));

        let line = render_tool_progress(
            "Bash",
            &json!({"command": "cargo test --workspace --all-features --release"}),
        );
        assert!(line.starts_with("🖥"));
    }

    #[test]
    fn unknown_tools_fall_back_generic() {
        let line = render_tool_progress("ExoticThing", &json!({}));
        assert_eq!(line, "🔧 ExoticThing");
    }

    #[test]
    fn mcp_tools_show_server_and_tool() {
        let line = render_tool_progress("mcp__github__create_issue", &json!({}));
        assert!(line.contains("github"));
        assert!(line.contains("create_issue"));
    }

    #[test]
    fn placeholder_caps_history_length() {
        let history: Vec<String> = (0..50).map(|i| format!("step {i}")).collect();
        let body = render_placeholder(&history);
        let n_bullets = body.matches("\n• ").count();
        assert_eq!(n_bullets, MAX_HISTORY_LINES);
        // Should keep the *latest* lines, not the earliest.
        assert!(body.contains("step 49"));
        assert!(!body.contains("step 0\n"));
    }

    #[test]
    fn short_path_keeps_short_paths_unchanged() {
        assert_eq!(short_path("src/x.rs"), "src/x.rs");
    }

    #[test]
    fn short_path_elides_long_paths() {
        // Must exceed MAX_PATH_CHARS (80) for elision to engage.
        let long = "a/very/deeply/nested/folder/with/a/long/structure/that/keeps/going/and/going/and/eventually/reaches/the/file.rs";
        assert!(long.chars().count() > MAX_PATH_CHARS);
        let s = short_path(long);
        assert!(s.starts_with("…/"), "got: {s}");
        assert!(s.ends_with("/file.rs"), "got: {s}");
    }

    #[test]
    fn truncate_handles_multibyte() {
        let s = "你好你好你好你好你好你好";
        let out = truncate(s, 5);
        assert_eq!(out.chars().count(), 5);
        assert!(out.ends_with('…'));
    }
}
