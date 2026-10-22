//! Minimal LXD REST client over the local Unix socket.
//!
//! Talks HTTP directly to the LXD daemon, so it works from a strictly
//! confined snap with only the `lxd` plug: no `lxc` binary is needed.
//! Sandbox lifecycle goes through the OpenShell gateway and its LXD compute
//! driver; this client covers health checks, the instance list, host folder
//! mounts and the debug console.

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

/// Config keys the OpenShell LXD driver sets on the instances it manages.
const KEY_MANAGED_BY: &str = "user.openshell.managed-by";
const KEY_ROLE: &str = "user.openshell.role";
const KEY_SANDBOX_ID: &str = "user.openshell.sandbox-id";
const KEY_SANDBOX_NAME: &str = "user.openshell.sandbox-name";

/// What an instance is within an OpenShell sandbox. The driver runs each
/// sandbox as a network-less workload plus a supervisor companion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Workload,
    Supervisor,
}

impl Instance {
    /// `None` for instances the OpenShell driver did not create.
    pub fn openshell_role(&self) -> Option<Role> {
        let config = &self.expanded_config;
        if config.get(KEY_MANAGED_BY).map(String::as_str) != Some("openshell") {
            return None;
        }
        match config.get(KEY_ROLE).map(String::as_str) {
            Some("workload") => Some(Role::Workload),
            Some("supervisor") => Some(Role::Supervisor),
            _ => None,
        }
    }

    pub fn openshell_sandbox_id(&self) -> Option<&str> {
        self.expanded_config.get(KEY_SANDBOX_ID).map(String::as_str)
    }

    /// The gateway-visible sandbox name, when the driver recorded one.
    pub fn openshell_sandbox_name(&self) -> Option<&str> {
        self.expanded_config
            .get(KEY_SANDBOX_NAME)
            .map(String::as_str)
            .filter(|name| !name.is_empty())
    }
}

/// One row of the sandbox list: an instance worth showing, and whether it is
/// really up.
#[derive(Debug, Clone, Copy)]
pub struct Listed<'a> {
    pub instance: &'a Instance,
    /// A managed workload counts as running only while its supervisor does
    /// too, since without the supervisor nothing enforces its policy.
    pub running: bool,
}

/// Collapses workload/supervisor pairs into one entry per sandbox and keeps
/// instances the driver does not manage as they are.
pub fn list_sandboxes(instances: &[Instance]) -> Vec<Listed<'_>> {
    let running_supervisors: std::collections::HashSet<&str> = instances
        .iter()
        .filter(|i| i.openshell_role() == Some(Role::Supervisor) && i.status == "Running")
        .filter_map(Instance::openshell_sandbox_id)
        .collect();
    instances
        .iter()
        .filter_map(|instance| match instance.openshell_role() {
            Some(Role::Supervisor) => None,
            Some(Role::Workload) => Some(Listed {
                instance,
                running: instance.status == "Running"
                    && instance
                        .openshell_sandbox_id()
                        .is_some_and(|id| running_supervisors.contains(id)),
            }),
            None => Some(Listed {
                instance,
                running: instance.status == "Running",
            }),
        })
        .collect()
}

/// A host directory to mount into an instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Disk {
    pub source: String,
    pub path: String,
    pub readonly: bool,
}

/// The name the app gives the device that mounts something at `path`, such
/// as `mount-sandbox-data` for `/sandbox/data`.
pub fn mount_device_name(path: &str) -> String {
    let mut name = String::from("mount");
    for c in path.chars() {
        if c.is_ascii_alphanumeric() {
            name.push(c.to_ascii_lowercase());
        } else if !name.ends_with('-') {
            name.push('-');
        }
    }
    name.trim_end_matches('-').to_owned()
}

/// CPU cores from `limits.cpu` (a count, or a list or range of CPUs) or the
/// driver's `limits.cpu.allowance` quota (`200ms/100ms` is two cores).
pub fn parse_cpu_limit(config: &HashMap<String, String>) -> Option<u32> {
    if let Some(cpu) = config.get("limits.cpu") {
        if let Ok(count) = cpu.trim().parse() {
            return Some(count);
        }
        let mut count = 0;
        for part in cpu.split(',') {
            count += match part.split_once('-') {
                Some((a, b)) => b.trim().parse::<u32>().ok()? + 1 - a.trim().parse::<u32>().ok()?,
                None => 1,
            };
        }
        return Some(count);
    }
    let allowance = config.get("limits.cpu.allowance")?;
    let (quota, period) = allowance.split_once('/')?;
    let ms = |v: &str| v.trim().strip_suffix("ms")?.parse::<f64>().ok();
    let cores = ms(quota)? / ms(period)?;
    (cores > 0.0).then(|| cores.ceil() as u32)
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

    /// Bind-mounts a host directory into an instance as a `disk` device.
    /// `shift` maps file ownership into the container's ID range, so an
    /// unprivileged sandbox user can use the files it is given.
    pub fn add_disk(&self, instance: &str, device: &str, disk: &Disk) -> Result<(), String> {
        let body = json!({ "devices": { device: {
            "type": "disk",
            "source": disk.source,
            "path": disk.path,
            "readonly": if disk.readonly { "true" } else { "false" },
            "shift": "true",
        }}});
        self.request_and_wait("PATCH", &format!("/1.0/instances/{instance}"), &body)
    }

    /// Removes a device the instance defines itself. Devices that come from a
    /// profile are not the instance's to remove.
    pub fn remove_device(&self, instance: &str, device: &str) -> Result<(), String> {
        let path = format!("/1.0/instances/{instance}");
        let mut config = self.request("GET", &path, None)?;
        let removed = config["devices"]
            .as_object_mut()
            .and_then(|devices| devices.remove(device));
        if removed.is_none() {
            return Err(format!(
                "{device} is not a device of {instance}; it may come from a profile"
            ));
        }
        self.request_and_wait("PUT", &path, &config)
    }

    /// Writes `len` bytes from `content` to `path` in `instance`, as a file
    /// owned by root with `mode`, replacing any file already there. The
    /// parent directory must exist. Unlike a mount, this works on the
    /// instance's own root filesystem, outside any policy inside it.
    pub fn push_file(
        &self,
        instance: &str,
        path: &str,
        mode: u32,
        content: &mut impl Read,
        len: u64,
    ) -> Result<(), String> {
        let mut stream = self.connect()?;
        // LXD answers once it has stored the whole file.
        stream
            .set_read_timeout(Some(Duration::from_secs(120)))
            .map_err(|e| e.to_string())?;
        let head = format!(
            "POST /1.0/instances/{instance}/files?path={} HTTP/1.0\r\nHost: lxd\r\n\
             Content-Type: application/octet-stream\r\nContent-Length: {len}\r\n\
             X-LXD-type: file\r\nX-LXD-uid: 0\r\nX-LXD-gid: 0\r\n\
             X-LXD-mode: {mode:04o}\r\nX-LXD-write: overwrite\r\n\r\n",
            query_escape(path)
        );
        stream
            .write_all(head.as_bytes())
            .map_err(|e| e.to_string())?;
        let copied = std::io::copy(&mut content.take(len), &mut stream)
            .map_err(|e| format!("sending {path}: {e}"))?;
        if copied != len {
            return Err(format!("{path}: only {copied} of {len} bytes to send"));
        }
        let envelope = read_envelope(&mut stream)?;
        match envelope["type"].as_str() {
            Some("error") => Err(envelope["error"]
                .as_str()
                .unwrap_or("unknown LXD error")
                .to_owned()),
            _ => Ok(()),
        }
    }

    /// Sends a request that may run as a background operation, and waits for
    /// that operation to finish.
    fn request_and_wait(&self, method: &str, path: &str, body: &Value) -> Result<(), String> {
        let (kind, envelope) = self.send(method, path, Some(body))?;
        if kind != "async" {
            return Ok(());
        }
        let operation = envelope["operation"]
            .as_str()
            .ok_or("async response has no operation")?;
        let done = self.request("GET", &format!("{operation}/wait?timeout=30"), None)?;
        match done["status_code"].as_i64() {
            Some(200) => Ok(()),
            _ => Err(done["err"]
                .as_str()
                .filter(|e| !e.is_empty())
                .unwrap_or("LXD operation failed")
                .to_owned()),
        }
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
        let (_, envelope) = self.send(method, path, body)?;
        Ok(envelope["metadata"].clone())
    }

    /// Sends one request and returns the response type (`sync` or `async`)
    /// with the whole envelope; LXD errors become `Err`.
    fn send(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
    ) -> Result<(String, Value), String> {
        let mut stream = self.connect()?;
        let body = body.map(Value::to_string).unwrap_or_default();
        let request = format!(
            "{method} {path} HTTP/1.0\r\nHost: lxd\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        stream
            .write_all(request.as_bytes())
            .map_err(|e| e.to_string())?;

        let envelope = read_envelope(&mut stream)?;
        match envelope["type"].as_str() {
            Some("error") => Err(envelope["error"]
                .as_str()
                .unwrap_or("unknown LXD error")
                .to_owned()),
            kind => Ok((kind.unwrap_or_default().to_owned(), envelope)),
        }
    }
}

/// Reads an HTTP/1.0 response to its end and parses LXD's JSON envelope.
fn read_envelope(stream: &mut UnixStream) -> Result<Value, String> {
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .map_err(|e| e.to_string())?;
    let (_, body) = response
        .split_once("\r\n\r\n")
        .ok_or("malformed HTTP response")?;
    serde_json::from_str(body).map_err(|e| format!("bad JSON from LXD: {e}"))
}

/// Percent-encodes a path for a query string; `/` stays readable.
fn query_escape(value: &str) -> String {
    value
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
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
    fn escapes_query_paths() {
        assert_eq!(
            query_escape("/usr/local/bin/claude"),
            "/usr/local/bin/claude"
        );
        assert_eq!(query_escape("/a b&c"), "/a%20b%26c");
    }

    #[test]
    fn parses_memory_limits() {
        assert_eq!(parse_memory_mb("4GiB"), Some(4096));
        assert_eq!(parse_memory_mb("512MiB"), Some(512));
        assert_eq!(parse_memory_mb("1GB"), Some(954));
        assert_eq!(parse_memory_mb("50%"), None);
    }

    fn instance(name: &str, status: &str, config: &[(&str, &str)]) -> Instance {
        Instance {
            name: name.into(),
            status: status.into(),
            kind: "container".into(),
            profiles: vec![],
            expanded_config: config
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
            expanded_devices: HashMap::new(),
            state: None,
        }
    }

    fn managed(name: &str, status: &str, role: &str, id: &str) -> Instance {
        instance(
            name,
            status,
            &[
                (KEY_MANAGED_BY, "openshell"),
                (KEY_ROLE, role),
                (KEY_SANDBOX_ID, id),
                (KEY_SANDBOX_NAME, "my-agent"),
            ],
        )
    }

    #[test]
    fn folds_supervisors_into_their_sandbox() {
        let instances = [
            managed("openshell-a", "Running", "workload", "a"),
            managed("openshell-sup-a", "Running", "supervisor", "a"),
            managed("openshell-b", "Running", "workload", "b"),
            managed("openshell-sup-b", "Stopped", "supervisor", "b"),
            managed("openshell-c", "Stopped", "workload", "c"),
            instance("plain", "Running", &[]),
        ];
        let listed = list_sandboxes(&instances);
        let names: Vec<&str> = listed.iter().map(|l| l.instance.name.as_str()).collect();
        assert_eq!(
            names,
            ["openshell-a", "openshell-b", "openshell-c", "plain"]
        );
        let running: Vec<bool> = listed.iter().map(|l| l.running).collect();
        assert_eq!(running, [true, false, false, true]);
        assert_eq!(
            listed[0].instance.openshell_sandbox_name(),
            Some("my-agent")
        );
        assert_eq!(listed[3].instance.openshell_role(), None);
    }

    #[test]
    fn ignores_foreign_role_keys() {
        let impostor = instance("x", "Running", &[(KEY_ROLE, "supervisor")]);
        assert_eq!(impostor.openshell_role(), None);
        assert_eq!(list_sandboxes(&[impostor]).len(), 1);
    }

    #[test]
    fn names_mount_devices_after_their_path() {
        assert_eq!(
            mount_device_name("/sandbox/My Data"),
            "mount-sandbox-my-data"
        );
        assert_eq!(mount_device_name("/"), "mount");
    }

    #[test]
    fn reads_cpu_limits_in_every_form() {
        let config = |k: &str, v: &str| HashMap::from([(k.to_owned(), v.to_owned())]);
        assert_eq!(parse_cpu_limit(&config("limits.cpu", "4")), Some(4));
        assert_eq!(parse_cpu_limit(&config("limits.cpu", "0-3,6")), Some(5));
        assert_eq!(
            parse_cpu_limit(&config("limits.cpu.allowance", "200ms/100ms")),
            Some(2)
        );
        assert_eq!(
            parse_cpu_limit(&config("limits.cpu.allowance", "50ms/100ms")),
            Some(1)
        );
        assert_eq!(
            parse_cpu_limit(&config("limits.cpu.allowance", "50%")),
            None
        );
        assert_eq!(parse_cpu_limit(&HashMap::new()), None);
    }

    #[test]
    fn missing_socket_is_unavailable() {
        let status = Client::new("/nonexistent/lxd.sock").status();
        assert!(!status.is_active());
    }
}
