//! Per-session ring buffer of raw PTY bytes.
//!
//! On every append we scan the (overlap-extended) new region for ANSI
//! anchor sequences that semantically reset the screen, and drain
//! everything before the latest such anchor. This is the "buffer trimming"
//! described in PLAN.md §4.6 — without it, reattaching after `cls` /
//! after claude exits its alt screen would replay stale frames.
//!
//! Anchors recognised:
//!   * `ESC[?1049h` — enter alt screen
//!   * `ESC[?1049l` — leave alt screen
//!   * `ESC[2J`     — erase entire display
//!   * `ESC[3J`     — erase scrollback
//!
//! The anchor itself is **kept** at the start of the trimmed buffer so
//! the next viewer's terminal transitions into the same screen state as
//! the source PTY.

use std::collections::VecDeque;

const TRIM_ANCHORS: &[&[u8]] = &[
    b"\x1b[?1049h",
    b"\x1b[?1049l",
    b"\x1b[2J",
    b"\x1b[3J",
];

/// Length of the longest anchor — used to size the cross-chunk overlap
/// when scanning, so a sequence that straddles two appends is still found.
const MAX_ANCHOR_LEN: usize = 8;

pub struct RingBuffer {
    buf: VecDeque<u8>,
    cap: usize,
}

impl RingBuffer {
    pub fn new(cap: usize) -> Self {
        assert!(cap > 0, "ring cap must be > 0");
        Self {
            buf: VecDeque::with_capacity(cap),
            cap,
        }
    }

    pub fn append(&mut self, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        let prev_len = self.buf.len();
        self.append_raw(data);

        // Determine where the newly-appended region starts in the post-append
        // buffer. If `data` was bigger than the cap, the front was wiped and
        // only the tail of `data` remains — the new region is the whole buf.
        let new_region_start = self.buf.len().saturating_sub(data.len().min(self.cap));

        // Scan from a few bytes before the boundary to catch anchors split
        // across two appends.
        let scan_lo = new_region_start
            .saturating_sub(MAX_ANCHOR_LEN - 1)
            .min(prev_len);
        let scan_hi = self.buf.len();

        let mut latest: Option<usize> = None;
        for pos in scan_lo..scan_hi {
            for seq in TRIM_ANCHORS {
                if pos + seq.len() <= scan_hi && self.matches_at(pos, seq) {
                    latest = Some(pos);
                }
            }
        }
        if let Some(anchor) = latest {
            self.buf.drain(..anchor);
        }
    }

    fn append_raw(&mut self, data: &[u8]) {
        if data.len() >= self.cap {
            self.buf.clear();
            self.buf.extend(&data[data.len() - self.cap..]);
            return;
        }
        let overflow = (self.buf.len() + data.len()).saturating_sub(self.cap);
        if overflow > 0 {
            self.buf.drain(..overflow);
        }
        self.buf.extend(data);
    }

    fn matches_at(&self, start: usize, seq: &[u8]) -> bool {
        seq.iter()
            .enumerate()
            .all(|(i, &b)| self.buf.get(start + i) == Some(&b))
    }

    pub fn snapshot(&self) -> Vec<u8> {
        let (a, b) = self.buf.as_slices();
        let mut v = Vec::with_capacity(a.len() + b.len());
        v.extend_from_slice(a);
        v.extend_from_slice(b);
        v
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn clear(&mut self) {
        self.buf.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn under_cap_kept_intact() {
        let mut r = RingBuffer::new(10);
        r.append(b"hello");
        assert_eq!(r.snapshot(), b"hello");
    }

    #[test]
    fn exact_cap_kept_intact() {
        let mut r = RingBuffer::new(5);
        r.append(b"hello");
        assert_eq!(r.snapshot(), b"hello");
    }

    #[test]
    fn overflow_drops_oldest() {
        let mut r = RingBuffer::new(5);
        r.append(b"abc");
        r.append(b"defg");
        assert_eq!(r.snapshot(), b"cdefg");
    }

    #[test]
    fn single_oversized_keeps_tail() {
        let mut r = RingBuffer::new(5);
        r.append(b"hellothere");
        assert_eq!(r.snapshot(), b"there");
    }

    #[test]
    fn tail_after_many_appends() {
        let mut r = RingBuffer::new(8);
        for chunk in [b"aa".as_slice(), b"bb", b"cc", b"dd", b"ee"] {
            r.append(chunk);
        }
        assert_eq!(r.snapshot(), b"bbccddee");
    }

    #[test]
    fn empty_append_noop() {
        let mut r = RingBuffer::new(5);
        r.append(b"abc");
        r.append(b"");
        assert_eq!(r.snapshot(), b"abc");
    }

    // Trim tests --------------------------------------------------------

    #[test]
    fn trim_on_clear_screen() {
        let mut r = RingBuffer::new(1024);
        r.append(b"junk before clear");
        r.append(b"\x1b[2Jclean");
        assert_eq!(r.snapshot(), b"\x1b[2Jclean");
    }

    #[test]
    fn trim_on_clear_with_scrollback() {
        let mut r = RingBuffer::new(1024);
        r.append(b"old stuff");
        r.append(b"\x1b[3Jfresh");
        assert_eq!(r.snapshot(), b"\x1b[3Jfresh");
    }

    #[test]
    fn trim_on_alt_screen_enter() {
        let mut r = RingBuffer::new(1024);
        r.append(b"main screen");
        r.append(b"\x1b[?1049hin alt");
        assert_eq!(r.snapshot(), b"\x1b[?1049hin alt");
    }

    #[test]
    fn trim_on_alt_screen_leave() {
        let mut r = RingBuffer::new(1024);
        r.append(b"\x1b[?1049halt content");
        r.append(b"\x1b[?1049lback to main");
        assert_eq!(r.snapshot(), b"\x1b[?1049lback to main");
    }

    #[test]
    fn trim_uses_latest_anchor_when_multiple() {
        let mut r = RingBuffer::new(1024);
        r.append(b"prefix\x1b[2Jmid\x1b[3Jtail");
        assert_eq!(r.snapshot(), b"\x1b[3Jtail");
    }

    #[test]
    fn trim_anchor_split_across_appends() {
        let mut r = RingBuffer::new(1024);
        r.append(b"junk\x1b[?10");
        r.append(b"49hin alt");
        assert_eq!(r.snapshot(), b"\x1b[?1049hin alt");
    }

    #[test]
    fn trim_anchor_at_buffer_start_is_noop() {
        let mut r = RingBuffer::new(1024);
        // Anchor lands at index 0 — drain(..0) is a no-op, all bytes kept.
        r.append(b"\x1b[2Jonly");
        assert_eq!(r.snapshot(), b"\x1b[2Jonly");
    }

    #[test]
    fn trim_does_not_match_partial_sequences() {
        let mut r = RingBuffer::new(1024);
        r.append(b"\x1b[2X not a clear");
        assert_eq!(r.snapshot(), b"\x1b[2X not a clear");
    }
}
