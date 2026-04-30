//! Cross-platform terminal mode handling: raw stdin + VT stdout, plus
//! a size query helper used by the resize poller.
//!
//! Backed by the `crossterm` crate, which on Windows toggles
//! `ENABLE_VIRTUAL_TERMINAL_INPUT` / `ENABLE_VIRTUAL_TERMINAL_PROCESSING`
//! and clears `ENABLE_LINE_INPUT|ENABLE_ECHO_INPUT|ENABLE_PROCESSED_INPUT`
//! the same way the previous Win32 implementation did, and on Unix
//! drives `termios` to enter cbreak mode.

use anyhow::{Context, Result};

/// RAII guard that restores cooked mode on drop. Crossterm's raw-mode
/// API is process-global (not handle-bound), so this guard only needs
/// to remember whether *we* turned it on — the underlying terminal
/// state is handled by `crossterm::terminal::{enable,disable}_raw_mode`.
pub struct RawModeGuard {
    /// True if we successfully entered raw mode. False indicates the
    /// guard is a no-op (e.g. constructed in a test or when stdin is
    /// not a TTY); dropping it then must NOT call disable, because
    /// some other code may have raw mode active.
    active: bool,
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        if self.active {
            let _ = crossterm::terminal::disable_raw_mode();
        }
    }
}

pub fn enter_raw_mode() -> Result<RawModeGuard> {
    crossterm::terminal::enable_raw_mode().context("enable raw mode")?;
    Ok(RawModeGuard { active: true })
}

/// Placeholder handle preserved for API compatibility with the old
/// Win32-backed implementation. Crossterm's `terminal::size()` doesn't
/// take a handle (it queries the controlling TTY directly), but
/// callers in `main.rs` thread `SendHandle` through resize-poller
/// closures, so we keep the type as a zero-size unit-like struct.
#[derive(Copy, Clone)]
pub struct SendHandle;

pub fn stdout_send_handle() -> Result<SendHandle> {
    Ok(SendHandle)
}

/// Returns the visible viewport size as (cols, rows).
pub fn query_size(_h: SendHandle) -> Result<(u16, u16)> {
    let (cols, rows) = crossterm::terminal::size().context("crossterm::terminal::size")?;
    Ok((cols.max(1), rows.max(1)))
}
