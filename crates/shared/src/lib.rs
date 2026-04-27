//! Shared protocol types between broker, viewers, and (later) IM bots / hooks.

pub mod config;
pub mod frame;
pub mod http;

pub const PIPE_NAME: &str = r"\\.\pipe\claude-broker";

/// Default broker HTTP control plane address. Hooks read
/// `AGENT_BROKER_URL` first; this is the fallback.
pub const DEFAULT_BROKER_URL: &str = "http://127.0.0.1:8765";
