//! Viewer ↔ broker frame protocol.
//!
//! Wire format:
//! ```text
//! +--------+--------------+----------------+
//! | u8 tag | u32 len (BE) | payload (len B)|
//! +--------+--------------+----------------+
//! ```
//!
//! Phase 3 uses three tags. Higher tags are reserved for later phases
//! (HELLO=0x03, ATTACH=0x05, CONTROL=0x06, EVENT=0x07 — see PLAN.md §3.1).

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const TAG_PTY_DATA: u8 = 0x01;
pub const TAG_RESIZE: u8 = 0x02;
pub const TAG_HELLO: u8 = 0x03;
pub const TAG_REPLAY_END: u8 = 0x04;
pub const TAG_CONTROL: u8 = 0x06;

/// Control commands carried in a TAG_CONTROL payload (JSON
/// `{"cmd":"..."}` per PLAN §3.1). Phase 4 hardcodes the `default`
/// session as the target; multi-session targeting comes in Phase 7.5.
pub const CTRL_INTERRUPT: &str = "interrupt";
pub const CTRL_RESTART: &str = "restart-claude";
pub const CTRL_SHUTDOWN: &str = "shutdown";

/// Hard cap on a single frame's payload. Protects against a hostile or
/// buggy peer trying to DoS the allocator. PTY chunks are typically <16KB;
/// the only large frame is the replay snapshot which is bounded by the
/// ring buffer cap (currently 512KB).
pub const MAX_PAYLOAD: usize = 1 << 20;

const HEADER_LEN: usize = 5;

pub async fn read_frame<R>(r: &mut R) -> std::io::Result<(u8, Vec<u8>)>
where
    R: AsyncRead + Unpin + ?Sized,
{
    let mut hdr = [0u8; HEADER_LEN];
    r.read_exact(&mut hdr).await?;
    let tag = hdr[0];
    let len = u32::from_be_bytes([hdr[1], hdr[2], hdr[3], hdr[4]]) as usize;
    if len > MAX_PAYLOAD {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("frame payload too large: {len} > {MAX_PAYLOAD}"),
        ));
    }
    let mut payload = vec![0u8; len];
    if len > 0 {
        r.read_exact(&mut payload).await?;
    }
    Ok((tag, payload))
}

pub async fn write_frame<W>(w: &mut W, tag: u8, payload: &[u8]) -> std::io::Result<()>
where
    W: AsyncWrite + Unpin + ?Sized,
{
    if payload.len() > MAX_PAYLOAD {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("frame payload too large: {} > {MAX_PAYLOAD}", payload.len()),
        ));
    }
    let mut hdr = [0u8; HEADER_LEN];
    hdr[0] = tag;
    hdr[1..5].copy_from_slice(&(payload.len() as u32).to_be_bytes());
    w.write_all(&hdr).await?;
    if !payload.is_empty() {
        w.write_all(payload).await?;
    }
    w.flush().await?;
    Ok(())
}

/// Pack a frame into a freshly-allocated `Vec<u8>` for transports
/// that prefer self-contained byte buffers (e.g. WebSocket binary
/// messages). Mirrors `write_frame`'s wire format exactly so the
/// receiver can `decode_frame` regardless of which writer was used.
pub fn encode_frame(tag: u8, payload: &[u8]) -> std::io::Result<Vec<u8>> {
    if payload.len() > MAX_PAYLOAD {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("frame payload too large: {} > {MAX_PAYLOAD}", payload.len()),
        ));
    }
    let mut buf = Vec::with_capacity(HEADER_LEN + payload.len());
    buf.push(tag);
    buf.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    buf.extend_from_slice(payload);
    Ok(buf)
}

/// Parse a single frame out of a self-contained byte buffer
/// (`encode_frame`'s inverse). Used on the receive side of
/// transports where each delivered chunk IS one whole frame.
pub fn decode_frame(buf: &[u8]) -> std::io::Result<(u8, &[u8])> {
    if buf.len() < HEADER_LEN {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "short frame",
        ));
    }
    let tag = buf[0];
    let len = u32::from_be_bytes([buf[1], buf[2], buf[3], buf[4]]) as usize;
    if len > MAX_PAYLOAD {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("frame payload too large: {len} > {MAX_PAYLOAD}"),
        ));
    }
    if buf.len() != HEADER_LEN + len {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "frame length mismatch: declared {} bytes, message has {}",
                len,
                buf.len() - HEADER_LEN
            ),
        ));
    }
    Ok((tag, &buf[HEADER_LEN..]))
}

pub fn encode_resize(cols: u16, rows: u16) -> [u8; 4] {
    let mut p = [0u8; 4];
    p[0..2].copy_from_slice(&cols.to_be_bytes());
    p[2..4].copy_from_slice(&rows.to_be_bytes());
    p
}

pub fn decode_resize(payload: &[u8]) -> Option<(u16, u16)> {
    if payload.len() != 4 {
        return None;
    }
    let cols = u16::from_be_bytes([payload[0], payload[1]]);
    let rows = u16::from_be_bytes([payload[2], payload[3]]);
    Some((cols, rows))
}

/// Encode a CONTROL command as JSON `{"cmd":"<cmd>"}`. Hand-rolled to
/// avoid pulling serde_json into the `shared` crate for one tiny shape.
pub fn encode_control(cmd: &str) -> Vec<u8> {
    let mut buf = Vec::with_capacity(cmd.len() + 10);
    buf.extend_from_slice(br#"{"cmd":""#);
    buf.extend_from_slice(cmd.as_bytes());
    buf.extend_from_slice(br#""}"#);
    buf
}

/// Decode a CONTROL payload. Accepts the exact shape produced by
/// `encode_control` — anything else returns None. Tolerates no
/// whitespace; this is a private wire format, not a general JSON parser.
pub fn decode_control(payload: &[u8]) -> Option<&str> {
    let s = std::str::from_utf8(payload).ok()?;
    let inner = s.strip_prefix(r#"{"cmd":""#)?;
    inner.strip_suffix(r#""}"#)
}

/// HELLO frame payload — sent by every viewer immediately after pipe
/// connect, identifies the client and selects which session to attach
/// to. Phase 7.5 only consumes the `session` field; client_kind/mode
/// become meaningful when IM bots arrive in Phase 6.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct HelloPayload {
    pub client_id: String,
    pub client_kind: String,
    pub mode: String,
    pub session: Option<String>,
}

impl Default for HelloPayload {
    fn default() -> Self {
        Self {
            client_id: String::new(),
            client_kind: "terminal".to_string(),
            mode: "rw".to_string(),
            session: None,
        }
    }
}

pub fn encode_hello(p: &HelloPayload) -> Vec<u8> {
    serde_json::to_vec(p).expect("serialise HelloPayload")
}

pub fn decode_hello(payload: &[u8]) -> Option<HelloPayload> {
    serde_json::from_slice(payload).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    #[tokio::test]
    async fn roundtrip_pty_data() {
        let (mut a, mut b) = duplex(64 * 1024);
        write_frame(&mut a, TAG_PTY_DATA, b"hello").await.unwrap();
        let (tag, payload) = read_frame(&mut b).await.unwrap();
        assert_eq!(tag, TAG_PTY_DATA);
        assert_eq!(payload, b"hello");
    }

    #[tokio::test]
    async fn roundtrip_empty_payload() {
        let (mut a, mut b) = duplex(64);
        write_frame(&mut a, TAG_REPLAY_END, &[]).await.unwrap();
        let (tag, payload) = read_frame(&mut b).await.unwrap();
        assert_eq!(tag, TAG_REPLAY_END);
        assert!(payload.is_empty());
    }

    #[tokio::test]
    async fn roundtrip_resize() {
        let (mut a, mut b) = duplex(64);
        let p = encode_resize(120, 30);
        write_frame(&mut a, TAG_RESIZE, &p).await.unwrap();
        let (tag, payload) = read_frame(&mut b).await.unwrap();
        assert_eq!(tag, TAG_RESIZE);
        assert_eq!(decode_resize(&payload), Some((120, 30)));
    }

    #[test]
    fn control_roundtrip_each_command() {
        for cmd in [CTRL_INTERRUPT, CTRL_RESTART, CTRL_SHUTDOWN] {
            let bytes = encode_control(cmd);
            assert_eq!(decode_control(&bytes), Some(cmd));
        }
    }

    #[test]
    fn hello_roundtrip_full() {
        let p = HelloPayload {
            client_id: "abc-123".into(),
            client_kind: "terminal".into(),
            mode: "rw".into(),
            session: Some("default".into()),
        };
        let bytes = encode_hello(&p);
        let back = decode_hello(&bytes).unwrap();
        assert_eq!(back.client_id, "abc-123");
        assert_eq!(back.client_kind, "terminal");
        assert_eq!(back.mode, "rw");
        assert_eq!(back.session.as_deref(), Some("default"));
    }

    #[test]
    fn hello_session_can_be_null() {
        let bytes = br#"{"client_kind":"terminal","mode":"rw","session":null}"#;
        let back = decode_hello(bytes).unwrap();
        assert!(back.session.is_none());
    }

    #[test]
    fn hello_missing_fields_fall_back_to_defaults() {
        let bytes = br#"{}"#;
        let back = decode_hello(bytes).unwrap();
        assert_eq!(back.client_kind, "terminal");
        assert_eq!(back.mode, "rw");
        assert!(back.session.is_none());
    }

    #[test]
    fn hello_rejects_invalid_json() {
        assert!(decode_hello(b"not json").is_none());
        assert!(decode_hello(b"").is_none());
    }

    #[test]
    fn control_rejects_garbage() {
        assert_eq!(decode_control(b"not json"), None);
        assert_eq!(decode_control(br#"{"cmd":"x"#), None); // missing close
        assert_eq!(decode_control(br#"{"x":"shutdown"}"#), None); // wrong key
    }

    #[tokio::test]
    async fn rejects_oversized_payload_on_read() {
        let (mut a, mut b) = duplex(64);
        // Hand-craft a header claiming 2 MiB, no payload — peer should bail.
        let mut hdr = [0u8; 5];
        hdr[0] = TAG_PTY_DATA;
        hdr[1..5].copy_from_slice(&((MAX_PAYLOAD as u32) + 1).to_be_bytes());
        a.write_all(&hdr).await.unwrap();
        let err = read_frame(&mut b).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }
}
