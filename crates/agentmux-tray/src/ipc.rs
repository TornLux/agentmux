//! Single-instance handshake + cross-process URL forwarding.
//!
//! Goal: when Windows protocol activation fires `agentmux://...` and
//! launches a *new* `agentmux-tray.exe` process, that process needs
//! to hand the URL off to the already-running tray and exit, so the
//! user's task tray never gets a duplicate icon.
//!
//! Implementation: the first instance binds a named pipe server. Any
//! subsequent launch tries to connect to that pipe — if the connect
//! succeeds, we're a secondary; we write our URL and quit. If the
//! connect fails because the pipe doesn't exist, we *become* the
//! primary by binding the listener.
//!
//! The pipe doubles as the singleton lock: only one process can own
//! the pipe name at a time. No separate mutex needed.

use std::time::Duration;

use anyhow::{Context, Result};
use interprocess::local_socket::traits::tokio::Listener as TokioListenerTrait;
use interprocess::local_socket::{
    GenericNamespaced, ListenerOptions, Name, ToNsName,
};
use tao::event_loop::EventLoopProxy;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::{debug, info, warn};

use crate::deeplink::DeepLink;
use crate::UserEvent;

const PIPE_NAME: &str = "agentmux-tray-cli.sock";
const FORWARD_TIMEOUT: Duration = Duration::from_secs(2);

/// Result of the singleton race.
pub enum InstanceRole {
    /// We bound the listener — we're authoritative for this user.
    /// The held `PipeListener` is consumed by `run_pipe_listener`.
    Primary(PipeListener),
    /// Another instance already had the pipe.
    Secondary,
}

/// Owned listener handle. Wraps the tokio-async listener type from
/// `interprocess` so the rest of the module doesn't have to thread
/// the type parameters through.
pub struct PipeListener {
    inner: interprocess::local_socket::tokio::Listener,
}

fn pipe_name() -> Result<Name<'static>> {
    PIPE_NAME
        .to_ns_name::<GenericNamespaced>()
        .context("encode pipe name")
}

/// Try to claim primary by binding the listener; on EADDRINUSE
/// (pipe already exists & someone is listening) return Secondary.
pub fn try_become_instance() -> InstanceRole {
    let name = match pipe_name() {
        Ok(n) => n,
        Err(e) => {
            // Should never happen; if it does, fail open as Secondary
            // so we never spawn duplicate trays.
            warn!("encode pipe name: {e:#}");
            return InstanceRole::Secondary;
        }
    };
    match ListenerOptions::new().name(name).create_tokio() {
        Ok(listener) => InstanceRole::Primary(PipeListener { inner: listener }),
        Err(e) => {
            debug!("local pipe bind failed (likely another tray running): {e}");
            InstanceRole::Secondary
        }
    }
}

/// Connect to the running primary and write the URL. Sync API because
/// secondary instances are short-lived: we block briefly, write, exit.
pub fn forward_deeplink(url: &str) -> Result<()> {
    use interprocess::local_socket::traits::Stream;
    use std::io::Write;

    let name = pipe_name()?;
    let mut conn = interprocess::local_socket::Stream::connect(name)
        .context("connect to existing tray pipe")?;
    conn.set_nonblocking(false).ok();
    conn.write_all(url.as_bytes()).context("write deeplink to pipe")?;
    conn.write_all(b"\n").ok();
    debug!("forwarded deeplink to running tray: {url}");
    let _ = FORWARD_TIMEOUT;
    Ok(())
}

/// Long-running task on the primary instance: accept connections,
/// read one URL per connection, dispatch to the event loop.
pub async fn run_pipe_listener(listener: PipeListener, proxy: EventLoopProxy<UserEvent>) {
    let listener = listener.inner;
    info!("pipe listener up on {}", PIPE_NAME);
    loop {
        let conn = match listener.accept().await {
            Ok(c) => c,
            Err(e) => {
                warn!("pipe accept: {e}");
                tokio::time::sleep(Duration::from_millis(500)).await;
                continue;
            }
        };
        let proxy = proxy.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_pipe_client(conn, proxy).await {
                warn!("pipe client: {e:#}");
            }
        });
    }
}

async fn handle_pipe_client(
    mut conn: interprocess::local_socket::tokio::Stream,
    proxy: EventLoopProxy<UserEvent>,
) -> Result<()> {
    // Cap inbound at a sane URL length — protocol activation never
    // sends more than a few KB even for absurd request ids. Read in
    // chunks until EOF or cap, then close.
    const MAX_LEN: usize = 8192;
    let mut buf = Vec::with_capacity(256);
    let mut tmp = [0u8; 256];
    loop {
        let n = match conn.read(&mut tmp).await {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => {
                warn!("pipe client read: {e}");
                break;
            }
        };
        buf.extend_from_slice(&tmp[..n]);
        if buf.len() >= MAX_LEN {
            break;
        }
    }
    let raw = String::from_utf8_lossy(&buf).trim().to_string();

    if raw.is_empty() {
        return Ok(());
    }
    debug!("received forwarded URL: {raw}");
    match DeepLink::parse(&raw) {
        Some(dl) => {
            let _ = proxy.send_event(UserEvent::DeepLinkReceived(dl));
        }
        None => warn!("forwarded URL did not parse as a deeplink: {raw}"),
    }
    let _ = AsyncWriteExt::shutdown(&mut conn).await;
    Ok(())
}
