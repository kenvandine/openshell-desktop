//! Minimal LXD REST client over the local Unix socket.
//!
//! Talks HTTP directly to the LXD daemon, so it works from a strictly
//! confined snap with only the `lxd` plug: no `lxc` binary is needed.
//! Sandbox lifecycle will go through the OpenShell gateway and its LXD
//! compute driver; this client covers health checks, the instance list and
//! the debug console.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};
use tungstenite::WebSocket;

pub mod console;

pub const SNAP_SOCKET: &str = "/var/snap/lxd/common/lxd/unix.socket";

const TIMEOUT: Duration = Duration::from_secs(5);

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

#[derive(Debug, Clone, Deserialize)]
pub struct Instance {
    pub name: String,
    pub status: String,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub profiles: Vec<String>,
    #[serde(default)]
    pub expanded_config: HashMap<String, String>,
    #[serde(default)]
    pub expanded_devices: HashMap<String, HashMap<String, String>>,
    pub state: Option<InstanceState>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct InstanceState {
    pub memory: Option<MemoryState>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MemoryState {
    #[serde(default)]
    pub usage: u64,
}

/// Secrets for the websockets of an interactive exec operation.
#[derive(Debug, Clone)]
pub struct ExecSession {
    pub operation_id: String,
    pub data_secret: String,
    pub control_secret: String,
}

#[derive(Debug, Clone)]
pub struct Client {
    socket: PathBuf,
}

impl Default for Client {
    fn default() -> Self {
        Self::new(SNAP_SOCKET)
    }
}

impl Client {
    pub fn new(socket: impl AsRef<Path>) -> Self {
        Self {
            socket: socket.as_ref().to_owned(),
        }
    }

    pub fn status(&self) -> LxdStatus {
        match self.request("GET", "/1.0", None) {
            Ok(metadata) => LxdStatus::Active {
                server_version: metadata["environment"]["server_version"]
                    .as_str()
                    .map(str::to_owned),
            },
            Err(reason) => LxdStatus::Unavailable { reason },
        }
    }

    pub fn instances(&self) -> Result<Vec<Instance>, String> {
        let metadata = self.request("GET", "/1.0/instances?recursion=2", None)?;
        serde_json::from_value(metadata).map_err(|e| format!("bad instance list: {e}"))
    }

    /// Starts an interactive command on a pseudo-terminal. LXD holds the
    /// process until both websockets from the returned session connect.
    pub fn exec_interactive(
        &self,
        instance: &str,
        command: &[&str],
        cols: u16,
        rows: u16,
    ) -> Result<ExecSession, String> {
        let body = json!({
            "command": command,
            "environment": { "TERM": "xterm-256color" },
            "interactive": true,
            "wait-for-websocket": true,
            "width": cols,
            "height": rows,
        });
        let metadata = self.request(
            "POST",
            &format!("/1.0/instances/{instance}/exec"),
            Some(&body),
        )?;

        let fds = &metadata["metadata"]["fds"];
        let secret = |fd: &str| {
            fds[fd]
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| format!("exec response has no secret for fd {fd}"))
        };
        Ok(ExecSession {
            operation_id: metadata["id"]
                .as_str()
                .ok_or("exec response has no operation id")?
                .to_owned(),
            data_secret: secret("0")?,
            control_secret: secret("control")?,
        })
    }

    pub fn websocket(
        &self,
        operation_id: &str,
        secret: &str,
    ) -> Result<WebSocket<UnixStream>, String> {
        let stream = self.connect()?;
        let url = format!("ws://lxd/1.0/operations/{operation_id}/websocket?secret={secret}");
        let (socket, _) = tungstenite::client(url, stream)
            .map_err(|e| format!("websocket handshake failed: {e}"))?;
        Ok(socket)
    }

    fn connect(&self) -> Result<UnixStream, String> {
        let stream = UnixStream::connect(&self.socket)
            .map_err(|e| format!("{}: {e}", self.socket.display()))?;
        stream
            .set_read_timeout(Some(TIMEOUT))
            .map_err(|e| e.to_string())?;
        stream
            .set_write_timeout(Some(TIMEOUT))
            .map_err(|e| e.to_string())?;
        Ok(stream)
    }

    /// Sends one request and returns the `metadata` of LXD's response envelope.
    fn request(&self, method: &str, path: &str, body: Option<&Value>) -> Result<Value, String> {
        let mut stream = self.connect()?;
        let body = body.map(Value::to_string).unwrap_or_default();
        let request = format!(
            "{method} {path} HTTP/1.0\r\nHost: lxd\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        stream
            .write_all(request.as_bytes())
            .map_err(|e| e.to_string())?;

        let mut response = String::new();
        stream
            .read_to_string(&mut response)
            .map_err(|e| e.to_string())?;
        let (_, body) = response
            .split_once("\r\n\r\n")
            .ok_or("malformed HTTP response")?;
        let envelope: Value =
            serde_json::from_str(body).map_err(|e| format!("bad JSON from LXD: {e}"))?;

        match envelope["type"].as_str() {
            Some("error") => Err(envelope["error"]
                .as_str()
                .unwrap_or("unknown LXD error")
                .to_owned()),
            _ => Ok(envelope["metadata"].clone()),
        }
    }
}

/// Parses LXD memory limits such as `4GiB`, `512MB` or `2048` (bytes) into MiB.
pub fn parse_memory_mb(value: &str) -> Option<u32> {
    let value = value.trim();
    let split = value
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(value.len());
    let (number, unit) = value.split_at(split);
    let number: f64 = number.parse().ok()?;
    let bytes = match unit {
        "" | "B" => 1.0,
        "kB" => 1e3,
        "MB" => 1e6,
        "GB" => 1e9,
        "TB" => 1e12,
        "KiB" => 1024.0,
        "MiB" => 1024.0 * 1024.0,
        "GiB" => 1024.0 * 1024.0 * 1024.0,
        "TiB" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    Some((number * bytes / (1024.0 * 1024.0)).round() as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_memory_limits() {
        assert_eq!(parse_memory_mb("4GiB"), Some(4096));
        assert_eq!(parse_memory_mb("512MiB"), Some(512));
        assert_eq!(parse_memory_mb("1GB"), Some(954));
        assert_eq!(parse_memory_mb("50%"), None);
    }

    #[test]
    fn missing_socket_is_unavailable() {
        let status = Client::new("/nonexistent/lxd.sock").status();
        assert!(!status.is_active());
    }
}
