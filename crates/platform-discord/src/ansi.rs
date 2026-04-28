//! Strip ANSI escape sequences and other terminal control codes from a
//! byte stream so the raw PTY ringbuffer is presentable as plain text.
//!
//! Scope: covers what claude code's TUI actually emits:
//!  * CSI sequences          `\x1b[ ... <final-byte>` (SGR colour, cursor
//!    movement, clear, mode toggles)
//!  * OSC sequences          `\x1b] ... ST` where ST is `\x07` or `\x1b\`
//!    (window title)
//!  * Single-char escapes    `\x1b<one byte>` (ESC c reset, ESC = etc.)
//!  * Other C0 controls (`\x00`-`\x08`, `\x0b`-`\x1f` minus `\n`/`\r`/`\t`)
//!    are dropped so e.g. backspace/bell/form-feed don't end up in the
//!    Discord output.
//!
//! Not handled: SS3 `\x1bO?`, DEC private mode reports — neither shows
//! up in claude code's output stream.
//!
//! Returns a UTF-8 String built via lossy decode, since the ringbuffer
//! is raw bytes that may straddle a multi-byte char at the boundary.

/// Strip control sequences and convert to UTF-8 (lossy on invalid bytes).
pub fn strip(bytes: &[u8]) -> String {
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == 0x1b {
            // ESC — what follows decides the sequence shape.
            i += 1;
            if i >= bytes.len() {
                break;
            }
            let next = bytes[i];
            match next {
                b'[' => {
                    // CSI: \x1b[ <0..n parameter / intermediate bytes> <final>
                    // final byte is 0x40-0x7e.
                    i += 1;
                    while i < bytes.len() {
                        let c = bytes[i];
                        i += 1;
                        if (0x40..=0x7e).contains(&c) {
                            break;
                        }
                    }
                }
                b']' => {
                    // OSC: \x1b] ... ST  (ST = BEL 0x07 or ESC \  i.e. 0x1b 0x5c)
                    i += 1;
                    while i < bytes.len() {
                        let c = bytes[i];
                        if c == 0x07 {
                            i += 1;
                            break;
                        }
                        if c == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'\\' {
                            i += 2;
                            break;
                        }
                        i += 1;
                    }
                }
                _ => {
                    // Single-char escape (ESC =, ESC c, ESC >, ...).
                    i += 1;
                }
            }
            continue;
        }
        if b < 0x20 && b != b'\n' && b != b'\r' && b != b'\t' {
            // Drop other C0 controls (backspace, bell, form-feed, ...).
            i += 1;
            continue;
        }
        out.push(b);
        i += 1;
    }
    // Lossy: ringbuffer is a circular byte buffer; may slice mid-char.
    String::from_utf8_lossy(&out).into_owned()
}

/// Take the last `n` lines (separated by `\n`) of `s`, dropping
/// fully-blank trailing lines first so the typical TUI cursor-on-an-
/// empty-row doesn't waste the budget.
pub fn last_lines(s: &str, n: usize) -> String {
    if n == 0 {
        return String::new();
    }
    // Normalize CR-only lines (TUI redraws use \r without \n).
    let normalized: String = s.replace('\r', "\n");
    let lines: Vec<&str> = normalized.lines().collect();
    let end = lines
        .iter()
        .rposition(|l| !l.trim().is_empty())
        .map(|i| i + 1)
        .unwrap_or(0);
    let start = end.saturating_sub(n);
    lines[start..end].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_csi() {
        let s = strip(b"\x1b[31mhello\x1b[0m world\n");
        assert_eq!(s, "hello world\n");
    }

    #[test]
    fn strips_cursor_moves() {
        let s = strip(b"abc\x1b[2Adef\x1b[5;3Hghi");
        assert_eq!(s, "abcdefghi");
    }

    #[test]
    fn strips_osc_bel() {
        let s = strip(b"\x1b]0;title\x07hello");
        assert_eq!(s, "hello");
    }

    #[test]
    fn strips_osc_st() {
        let s = strip(b"\x1b]0;title\x1b\\hello");
        assert_eq!(s, "hello");
    }

    #[test]
    fn keeps_newline_and_tab() {
        let s = strip(b"a\nb\tc\rd");
        assert_eq!(s, "a\nb\tc\rd");
    }

    #[test]
    fn drops_other_c0() {
        let s = strip(b"a\x07b\x08c\x0cd");
        assert_eq!(s, "abcd");
    }

    #[test]
    fn last_lines_basic() {
        let s = "a\nb\nc\nd\ne";
        assert_eq!(last_lines(s, 2), "d\ne");
        assert_eq!(last_lines(s, 10), "a\nb\nc\nd\ne");
    }

    #[test]
    fn last_lines_strips_trailing_blanks() {
        let s = "a\nb\nc\n\n\n";
        assert_eq!(last_lines(s, 2), "b\nc");
    }
}
