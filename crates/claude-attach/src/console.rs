//! Win32 console mode handling: raw stdin + VT stdout, plus a size query
//! helper used by the resize poller.

use anyhow::{Context, Result};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Console::{
    GetConsoleMode, GetConsoleScreenBufferInfo, GetStdHandle, SetConsoleMode, CONSOLE_MODE,
    CONSOLE_SCREEN_BUFFER_INFO, ENABLE_ECHO_INPUT, ENABLE_LINE_INPUT, ENABLE_PROCESSED_INPUT,
    ENABLE_VIRTUAL_TERMINAL_INPUT, ENABLE_VIRTUAL_TERMINAL_PROCESSING, STD_INPUT_HANDLE,
    STD_OUTPUT_HANDLE,
};

pub struct RawModeGuard {
    stdin: HANDLE,
    stdout: HANDLE,
    saved_in: CONSOLE_MODE,
    saved_out: CONSOLE_MODE,
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = SetConsoleMode(self.stdin, self.saved_in);
            let _ = SetConsoleMode(self.stdout, self.saved_out);
        }
    }
}

pub fn enter_raw_mode() -> Result<RawModeGuard> {
    unsafe {
        let stdin = GetStdHandle(STD_INPUT_HANDLE).context("GetStdHandle(stdin)")?;
        let stdout = GetStdHandle(STD_OUTPUT_HANDLE).context("GetStdHandle(stdout)")?;

        let mut saved_in = CONSOLE_MODE(0);
        GetConsoleMode(stdin, &mut saved_in).context("GetConsoleMode(stdin)")?;
        let mut saved_out = CONSOLE_MODE(0);
        GetConsoleMode(stdout, &mut saved_out).context("GetConsoleMode(stdout)")?;

        let new_in = (saved_in.0
            & !(ENABLE_ECHO_INPUT.0 | ENABLE_LINE_INPUT.0 | ENABLE_PROCESSED_INPUT.0))
            | ENABLE_VIRTUAL_TERMINAL_INPUT.0;
        SetConsoleMode(stdin, CONSOLE_MODE(new_in)).context("SetConsoleMode(stdin)")?;

        let new_out = saved_out.0 | ENABLE_VIRTUAL_TERMINAL_PROCESSING.0;
        SetConsoleMode(stdout, CONSOLE_MODE(new_out)).context("SetConsoleMode(stdout)")?;

        Ok(RawModeGuard {
            stdin,
            stdout,
            saved_in,
            saved_out,
        })
    }
}

/// Send-safe wrapper around a Win32 HANDLE so we can move it into a
/// tokio task. The underlying handle is process-global; sharing the value
/// between threads is safe as long as no single Win32 call mutates it
/// concurrently in a racy way (the Console API queries we use are read-
/// only / serialised by the kernel).
#[derive(Copy, Clone)]
pub struct SendHandle(pub HANDLE);

unsafe impl Send for SendHandle {}
unsafe impl Sync for SendHandle {}

pub fn stdout_send_handle() -> Result<SendHandle> {
    unsafe {
        let h = GetStdHandle(STD_OUTPUT_HANDLE).context("GetStdHandle(stdout)")?;
        Ok(SendHandle(h))
    }
}

/// Returns the visible viewport size as (cols, rows). Uses the window
/// rect, not the (possibly larger) screen buffer — that's what claude /
/// any TUI cares about for layout.
pub fn query_size(h: SendHandle) -> Result<(u16, u16)> {
    unsafe {
        let mut info = CONSOLE_SCREEN_BUFFER_INFO::default();
        GetConsoleScreenBufferInfo(h.0, &mut info).context("GetConsoleScreenBufferInfo")?;
        let cols = (info.srWindow.Right as i32 - info.srWindow.Left as i32 + 1).max(1) as u16;
        let rows = (info.srWindow.Bottom as i32 - info.srWindow.Top as i32 + 1).max(1) as u16;
        Ok((cols, rows))
    }
}
