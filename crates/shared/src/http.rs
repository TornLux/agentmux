//! Tiny synchronous HTTP/1.1 POST helper for short-lived hook binaries
//! talking to the broker on `127.0.0.1`.
//!
//! Deliberately no `reqwest`/`ureq`/`hyper` — those drag in TLS, async
//! runtimes, and 30-50 transitive crates that would dominate the size
//! of every hook executable. We only ever speak HTTP/1.1 to localhost,
//! so a hand-rolled writer suffices.

use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const IO_TIMEOUT: Duration = Duration::from_secs(5);

fn split_url(url: &str) -> io::Result<(&str, &str)> {
    let url_no_scheme = url
        .strip_prefix("http://")
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "url must start with http://"))?;
    let (host_port, path) = match url_no_scheme.find('/') {
        Some(idx) => (&url_no_scheme[..idx], &url_no_scheme[idx..]),
        None => (url_no_scheme, "/"),
    };
    Ok((host_port, path))
}

fn connect(host_port: &str) -> io::Result<TcpStream> {
    let addr = host_port
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::AddrNotAvailable, "no socket addr resolved"))?;
    let stream = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT)?;
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    Ok(stream)
}

/// Synchronous HTTP GET. Reads the full response (assumes the server
/// closes after sending — `Connection: close` is set in the request),
/// validates a 2xx status, and returns the body as a String. For
/// localhost-only use; no chunked-decode, no redirect handling.
pub fn get(url: &str) -> io::Result<String> {
    let (host_port, path) = split_url(url)?;
    let mut stream = connect(host_port)?;

    let req = format!(
        "GET {path} HTTP/1.1\r\n\
         Host: {host_port}\r\n\
         Accept: application/json\r\n\
         Connection: close\r\n\
         \r\n",
    );
    stream.write_all(req.as_bytes())?;
    stream.flush()?;

    let mut buf = Vec::new();
    stream.read_to_end(&mut buf)?;

    let s = std::str::from_utf8(&buf)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "non-utf8 response"))?;
    let body_start = s
        .find("\r\n\r\n")
        .map(|i| i + 4)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "no header/body separator"))?;
    let head = &s[..body_start - 4];
    if !head.starts_with("HTTP/1.1 2") && !head.starts_with("HTTP/1.0 2") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "non-2xx response: {}",
                head.lines().next().unwrap_or("(empty)")
            ),
        ));
    }
    Ok(s[body_start..].to_string())
}

pub fn post_json(url: &str, body: &str) -> io::Result<()> {
    let (host_port, path) = split_url(url)?;
    let mut stream = connect(host_port)?;

    let req = format!(
        "POST {path} HTTP/1.1\r\n\
         Host: {host_port}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {len}\r\n\
         Connection: close\r\n\
         \r\n\
         {body}",
        len = body.len(),
    );
    stream.write_all(req.as_bytes())?;
    stream.flush()?;

    // Just enough to inspect the status line.
    let mut buf = [0u8; 64];
    let n = stream.read(&mut buf).unwrap_or(0);
    let head = std::str::from_utf8(&buf[..n]).unwrap_or("");
    if !head.starts_with("HTTP/1.1 2") && !head.starts_with("HTTP/1.0 2") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "non-2xx response: {}",
                head.lines().next().unwrap_or("(empty)")
            ),
        ));
    }
    Ok(())
}

/// POST + return body as String, with caller-supplied I/O timeout.
/// Used by long-poll endpoints (PreToolUse approval) where the
/// server may take minutes to respond. `timeout` applies to BOTH
/// read and write deadlines on the socket; connect timeout stays at
/// the module default.
pub fn post_json_with_response(url: &str, body: &str, timeout: Duration) -> io::Result<String> {
    let (host_port, path) = split_url(url)?;
    let mut stream = connect(host_port)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;

    let req = format!(
        "POST {path} HTTP/1.1\r\n\
         Host: {host_port}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {len}\r\n\
         Connection: close\r\n\
         \r\n\
         {body}",
        len = body.len(),
    );
    stream.write_all(req.as_bytes())?;
    stream.flush()?;

    let mut buf = Vec::new();
    stream.read_to_end(&mut buf)?;

    let s = std::str::from_utf8(&buf)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "non-utf8 response"))?;
    let body_start = s
        .find("\r\n\r\n")
        .map(|i| i + 4)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "no header/body separator"))?;
    let head = &s[..body_start - 4];
    if !head.starts_with("HTTP/1.1 2") && !head.starts_with("HTTP/1.0 2") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "non-2xx response: {}",
                head.lines().next().unwrap_or("(empty)")
            ),
        ));
    }
    Ok(s[body_start..].to_string())
}
