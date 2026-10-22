//! Minimal LXD health probe over the local Unix socket.
//!
//! This only answers "is LXD reachable?" for the header status indicator.
//! Sandbox lifecycle goes through the OpenShell gateway and its LXD compute
//! driver, not this module.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

pub const SNAP_SOCKET: &str = "/var/snap/lxd/common/lxd/unix.socket";

#[derive(Debug, Clone)]
pub enum LxdStatus {
    Active { server_version: Option<String> },
    Unavailable { reason: String },
}

impl LxdStatus {
    pub fn is_active(&self) -> bool {
        matches!(self, LxdStatus::Active { .. })
    }
}

pub fn probe() -> LxdStatus {
    match get_server_info(SNAP_SOCKET) {
        Ok(body) => LxdStatus::Active {
            server_version: json_string_field(&body, "server_version"),
        },
        Err(reason) => LxdStatus::Unavailable { reason },
    }
}

fn get_server_info(socket: &str) -> Result<String, String> {
    let mut stream = UnixStream::connect(socket).map_err(|e| format!("{socket}: {e}"))?;
    let timeout = Some(Duration::from_secs(2));
    stream
        .set_read_timeout(timeout)
        .map_err(|e| e.to_string())?;
    stream
        .set_write_timeout(timeout)
        .map_err(|e| e.to_string())?;
    stream
        .write_all(b"GET /1.0 HTTP/1.0\r\nHost: lxd\r\n\r\n")
        .map_err(|e| e.to_string())?;

    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .map_err(|e| e.to_string())?;

    let status_line = response.lines().next().unwrap_or_default();
    if !status_line.contains(" 200 ") {
        return Err(format!("unexpected response: {status_line}"));
    }
    Ok(response
        .split_once("\r\n\r\n")
        .map(|(_, body)| body.to_owned())
        .unwrap_or_default())
}

/// Pulls a string field out of a JSON body without a JSON dependency.
/// Good enough for a health probe; real API calls should use serde.
fn json_string_field(body: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\":");
    let rest = body[body.find(&needle)? + needle.len()..].trim_start();
    let rest = rest.strip_prefix('"')?;
    Some(rest[..rest.find('"')?].to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_server_version() {
        let body = r#"{"metadata":{"environment":{"server_version": "6.5","x":1}}}"#;
        assert_eq!(
            json_string_field(body, "server_version").as_deref(),
            Some("6.5")
        );
        assert_eq!(json_string_field(body, "missing"), None);
    }

    #[test]
    fn missing_socket_is_unavailable() {
        assert!(get_server_info("/nonexistent/lxd.sock").is_err());
    }
}
