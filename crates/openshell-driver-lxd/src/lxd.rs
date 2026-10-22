//! Async LXD REST client over the local Unix socket.
//!
//! Talks HTTP straight to the daemon, so the driver needs only the snap `lxd`
//! plug and never the `lxc` binary. Every request is scoped to one LXD
//! project. Long-running calls (create, state changes, image import) return
//! LXD background operations, which this client waits on before returning.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use bytes::Bytes;
use futures::{Stream, StreamExt};
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full, StreamBody};
use hyper::body::Frame;
use hyper::{Request, StatusCode};
use hyper_util::rt::TokioIo;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::net::UnixStream;
use tokio_tungstenite::tungstenite::Message;

pub const SNAP_SOCKET: &str = "/var/snap/lxd/common/lxd/unix.socket";

/// How long LXD may take to finish one background operation.
const OPERATION_TIMEOUT_SECS: u64 = 600;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LxdError {
    /// The socket could not be reached or the connection broke.
    Connection(String),
    NotFound(String),
    /// LXD answered with an error envelope or failed operation.
    Api {
        status: u16,
        message: String,
    },
}

impl std::fmt::Display for LxdError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LxdError::Connection(message) => write!(f, "LXD unreachable: {message}"),
            LxdError::NotFound(message) => write!(f, "not found in LXD: {message}"),
            LxdError::Api { status, message } => write!(f, "LXD error ({status}): {message}"),
        }
    }
}

impl std::error::Error for LxdError {}

pub type Result<T> = std::result::Result<T, LxdError>;

type Body = BoxBody<Bytes, std::io::Error>;

#[derive(Debug, Clone, Deserialize)]
pub struct Instance {
    pub name: String,
    pub status: String,
    #[serde(default)]
    pub config: HashMap<String, String>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub last_used_at: Option<String>,
    #[serde(default)]
    pub profiles: Vec<String>,
    /// Devices set on the instance itself.
    #[serde(default)]
    pub devices: HashMap<String, HashMap<String, String>>,
    /// Devices after profiles are applied; what the instance really gets.
    #[serde(default)]
    pub expanded_devices: HashMap<String, HashMap<String, String>>,
}

/// Ownership and mode for a file pushed into an instance. The IDs are
/// inside the instance's user namespace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileMeta {
    pub uid: u32,
    pub gid: u32,
    pub mode: u32,
}

/// One lifecycle event from `GET /1.0/events`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LifecycleEvent {
    pub action: String,
    /// Instance name taken from the event source, when it is an instance.
    pub instance: Option<String>,
    pub project: String,
    pub timestamp: Option<String>,
}

pub type EventStream = Pin<Box<dyn Stream<Item = Result<LifecycleEvent>> + Send>>;

#[derive(Debug, Clone)]
pub struct Client {
    socket: PathBuf,
    project: String,
}

impl Client {
    pub fn new(socket: impl AsRef<Path>, project: impl Into<String>) -> Self {
        Self {
            socket: socket.as_ref().to_owned(),
            project: project.into(),
        }
    }

    pub fn project(&self) -> &str {
        &self.project
    }

    /// `GET /1.0`: server environment, API extensions and auth state.
    pub async fn server(&self) -> Result<Value> {
        self.get("/1.0").await
    }

    pub async fn instances(&self) -> Result<Vec<Instance>> {
        let metadata = self.get(&self.scoped("/1.0/instances?recursion=1")).await?;
        decode(metadata, "instance list")
    }

    pub async fn instance(&self, name: &str) -> Result<Option<Instance>> {
        match self
            .get(&self.scoped(&format!("/1.0/instances/{name}")))
            .await
        {
            Ok(metadata) => decode(metadata, "instance").map(Some),
            Err(LxdError::NotFound(_)) => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub async fn create_instance(&self, body: &Value) -> Result<()> {
        let metadata = self
            .send(
                "POST",
                &self.scoped("/1.0/instances"),
                json_body(body),
                None,
            )
            .await?;
        self.wait(&metadata).await.map(drop)
    }

    /// Merges `config` into the instance's local config.
    pub async fn patch_instance_config(
        &self,
        name: &str,
        config: &HashMap<String, String>,
    ) -> Result<()> {
        let body = json!({ "config": config });
        self.send(
            "PATCH",
            &self.scoped(&format!("/1.0/instances/{name}")),
            json_body(&body),
            None,
        )
        .await
        .map(drop)
    }

    /// Runs a state change (`start`, `stop`, ...) and waits for it to finish.
    pub async fn set_state(
        &self,
        name: &str,
        action: &str,
        force: bool,
        timeout: i64,
    ) -> Result<()> {
        let body = json!({ "action": action, "force": force, "timeout": timeout });
        let metadata = self
            .send(
                "PUT",
                &self.scoped(&format!("/1.0/instances/{name}/state")),
                json_body(&body),
                None,
            )
            .await?;
        self.wait(&metadata).await.map(drop)
    }

    pub async fn delete_instance(&self, name: &str) -> Result<()> {
        let metadata = self
            .send(
                "DELETE",
                &self.scoped(&format!("/1.0/instances/{name}")),
                empty_body(),
                None,
            )
            .await?;
        self.wait(&metadata).await.map(drop)
    }

    /// Reads a file from an instance's filesystem; works while it is stopped.
    pub async fn read_file(&self, name: &str, path: &str) -> Result<Option<Vec<u8>>> {
        let url = self.scoped(&format!(
            "/1.0/instances/{name}/files?path={}",
            percent_encode(path)
        ));
        match self.raw("GET", &url, empty_body(), None).await {
            Ok((StatusCode::OK, body)) => Ok(Some(body.to_vec())),
            Ok((StatusCode::NOT_FOUND, _)) | Err(LxdError::NotFound(_)) => Ok(None),
            Ok((status, body)) => Err(error_from_body(status, &body)),
            Err(error) => Err(error),
        }
    }

    /// Writes a file into an instance, replacing any existing one. Works on
    /// the root filesystem of a stopped instance and, once running, also on
    /// device mounts.
    pub async fn push_file(
        &self,
        name: &str,
        path: &str,
        content: &[u8],
        meta: FileMeta,
    ) -> Result<()> {
        self.push(name, path, "file", content, meta, false).await
    }

    /// Creates a directory if it is missing; an existing one is left alone.
    /// Parents must exist.
    pub async fn make_dir(&self, name: &str, path: &str, meta: FileMeta) -> Result<()> {
        self.push(name, path, "directory", &[], meta, false).await
    }

    /// Creates a directory, or fixes the owner and mode of an existing one.
    /// Parents must exist.
    pub async fn ensure_dir(&self, name: &str, path: &str, meta: FileMeta) -> Result<()> {
        self.push(name, path, "directory", &[], meta, true).await
    }

    async fn push(
        &self,
        name: &str,
        path: &str,
        kind: &str,
        content: &[u8],
        meta: FileMeta,
        modify_existing: bool,
    ) -> Result<()> {
        let url = self.scoped(&format!(
            "/1.0/instances/{name}/files?path={}",
            percent_encode(path)
        ));
        let mut headers = vec![
            ("X-LXD-uid", meta.uid.to_string()),
            ("X-LXD-gid", meta.gid.to_string()),
            ("X-LXD-mode", format!("{:04o}", meta.mode)),
            ("X-LXD-type", kind.to_owned()),
            ("X-LXD-write", "overwrite".to_owned()),
        ];
        if modify_existing {
            headers.push(("X-LXD-modify-perm", "mode,uid,gid".to_owned()));
        }
        let body = Full::new(Bytes::copy_from_slice(content))
            .map_err(|never| match never {})
            .boxed();
        let (status, bytes) = self
            .raw_with(
                "POST",
                &url,
                body,
                Some((content.len() as u64, "application/octet-stream")),
                &headers,
            )
            .await?;
        if status.is_success() {
            Ok(())
        } else {
            Err(error_from_body(status, &bytes))
        }
    }

    /// Creates an empty filesystem volume in `pool`. Existing is fine.
    pub async fn create_volume(&self, pool: &str, name: &str) -> Result<()> {
        // Both instances of a sandbox mount the volume.
        let body = json!({
            "name": name,
            "type": "custom",
            "content_type": "filesystem",
            "config": { "security.shared": "true" },
        });
        match self
            .send(
                "POST",
                &self.scoped(&format!("/1.0/storage-pools/{pool}/volumes/custom")),
                json_body(&body),
                None,
            )
            .await
        {
            Ok(_) | Err(LxdError::Api { status: 409, .. }) => Ok(()),
            Err(error) => Err(error),
        }
    }

    /// Deletes a custom volume; a missing one is fine.
    pub async fn delete_volume(&self, pool: &str, name: &str) -> Result<()> {
        match self
            .send(
                "DELETE",
                &self.scoped(&format!("/1.0/storage-pools/{pool}/volumes/custom/{name}")),
                empty_body(),
                None,
            )
            .await
        {
            Ok(_) | Err(LxdError::NotFound(_)) => Ok(()),
            Err(error) => Err(error),
        }
    }

    /// The managed network's IPv4 address (the host side of a bridge), for
    /// example `10.12.34.1`.
    pub async fn network_ipv4(&self, network: &str) -> Result<Option<String>> {
        let metadata = self.get(&format!("/1.0/networks/{network}")).await?;
        Ok(metadata["config"]["ipv4.address"]
            .as_str()
            .and_then(|cidr| cidr.split('/').next())
            .filter(|ip| ip.parse::<std::net::Ipv4Addr>().is_ok())
            .map(str::to_owned))
    }

    /// Returns the fingerprint behind an image alias, if the alias exists.
    pub async fn image_alias(&self, alias: &str) -> Result<Option<String>> {
        match self
            .get(&self.scoped(&format!("/1.0/images/aliases/{alias}")))
            .await
        {
            Ok(metadata) => Ok(metadata["target"].as_str().map(str::to_owned)),
            Err(LxdError::NotFound(_)) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Uploads a unified image tarball (`metadata.yaml` + `rootfs/`) and
    /// points `alias` at it. Returns the image fingerprint.
    pub async fn import_image(&self, tarball: &Path, alias: &str) -> Result<String> {
        let file = tokio::fs::File::open(tarball)
            .await
            .map_err(|e| LxdError::Connection(format!("{}: {e}", tarball.display())))?;
        let length = file
            .metadata()
            .await
            .map_err(|e| LxdError::Connection(e.to_string()))?
            .len();
        let frames = tokio_util::io::ReaderStream::new(file).map(|chunk| chunk.map(Frame::data));
        let body = BodyExt::boxed(StreamBody::new(frames));

        let metadata = self
            .send(
                "POST",
                &self.scoped("/1.0/images"),
                body,
                Some((length, "application/octet-stream")),
            )
            .await?;
        let operation = self.wait(&metadata).await?;
        let fingerprint = operation["metadata"]["fingerprint"]
            .as_str()
            .ok_or_else(|| api_error("image import returned no fingerprint"))?
            .to_owned();

        let alias_body = json!({ "name": alias, "target": fingerprint });
        match self
            .send(
                "POST",
                &self.scoped("/1.0/images/aliases"),
                json_body(&alias_body),
                None,
            )
            .await
        {
            Ok(_) => Ok(fingerprint),
            // Another import raced us to the same alias; theirs is identical.
            Err(LxdError::Api { status: 409, .. }) => Ok(fingerprint),
            Err(error) => Err(error),
        }
    }

    /// Subscribes to lifecycle events for this client's project.
    pub async fn lifecycle_events(&self) -> Result<EventStream> {
        let stream = self.connect().await?;
        let url = format!("ws://lxd{}", self.scoped("/1.0/events?type=lifecycle"));
        let (socket, _) = tokio_tungstenite::client_async(url, stream)
            .await
            .map_err(|e| LxdError::Connection(format!("event websocket: {e}")))?;
        let events = socket.filter_map(|message| async move {
            match message {
                Ok(Message::Text(text)) => parse_lifecycle_event(text.as_str()).map(Ok),
                Ok(Message::Binary(data)) => std::str::from_utf8(&data)
                    .ok()
                    .and_then(parse_lifecycle_event)
                    .map(Ok),
                Ok(_) => None,
                Err(e) => Some(Err(LxdError::Connection(format!("event websocket: {e}")))),
            }
        });
        Ok(Box::pin(events))
    }

    /// Adds the `project` query parameter to an API path.
    fn scoped(&self, path: &str) -> String {
        let separator = if path.contains('?') { '&' } else { '?' };
        format!("{path}{separator}project={}", percent_encode(&self.project))
    }

    async fn get(&self, path: &str) -> Result<Value> {
        self.send("GET", path, empty_body(), None).await
    }

    /// Waits for the background operation described by `metadata`, if any,
    /// and returns the finished operation.
    async fn wait(&self, metadata: &Value) -> Result<Value> {
        let Some(id) = metadata["id"].as_str() else {
            return Ok(metadata.clone());
        };
        let operation = self
            .get(&self.scoped(&format!(
                "/1.0/operations/{id}/wait?timeout={OPERATION_TIMEOUT_SECS}"
            )))
            .await?;
        match operation["status"].as_str() {
            Some("Success") => Ok(operation),
            Some("Running") | Some("Pending") => {
                Err(api_error(format!("operation {id} timed out")))
            }
            _ => Err(api_error(
                operation["err"]
                    .as_str()
                    .filter(|e| !e.is_empty())
                    .unwrap_or("operation failed"),
            )),
        }
    }

    /// Sends one request and returns the `metadata` of LXD's JSON envelope.
    async fn send(
        &self,
        method: &str,
        path: &str,
        body: Body,
        content: Option<(u64, &str)>,
    ) -> Result<Value> {
        let (status, bytes) = self.raw(method, path, body, content).await?;
        let envelope: Value =
            serde_json::from_slice(&bytes).map_err(|_| error_from_body(status, &bytes))?;
        if envelope["type"] == "error" || !status.is_success() {
            return Err(envelope_error(status, &envelope));
        }
        Ok(envelope["metadata"].clone())
    }

    async fn raw(
        &self,
        method: &str,
        path: &str,
        body: Body,
        content: Option<(u64, &str)>,
    ) -> Result<(StatusCode, Bytes)> {
        self.raw_with(method, path, body, content, &[]).await
    }

    async fn raw_with(
        &self,
        method: &str,
        path: &str,
        body: Body,
        content: Option<(u64, &str)>,
        headers: &[(&str, String)],
    ) -> Result<(StatusCode, Bytes)> {
        let stream = self.connect().await?;
        let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .map_err(|e| LxdError::Connection(e.to_string()))?;
        tokio::spawn(connection);

        let mut request = Request::builder()
            .method(method)
            .uri(path)
            .header("Host", "lxd");
        request = match content {
            Some((length, kind)) => request
                .header("Content-Type", kind)
                .header("Content-Length", length),
            None => request.header("Content-Type", "application/json"),
        };
        for (name, value) in headers {
            request = request.header(*name, value);
        }
        let request = request
            .body(body)
            .map_err(|e| LxdError::Connection(e.to_string()))?;

        let response = sender
            .send_request(request)
            .await
            .map_err(|e| LxdError::Connection(e.to_string()))?;
        let status = response.status();
        let bytes = response
            .into_body()
            .collect()
            .await
            .map_err(|e| LxdError::Connection(e.to_string()))?
            .to_bytes();
        if status == StatusCode::NOT_FOUND {
            return Err(LxdError::NotFound(path.to_owned()));
        }
        Ok((status, bytes))
    }

    async fn connect(&self) -> Result<UnixStream> {
        UnixStream::connect(&self.socket)
            .await
            .map_err(|e| LxdError::Connection(format!("{}: {e}", self.socket.display())))
    }
}

fn json_body(value: &Value) -> Body {
    Full::new(Bytes::from(value.to_string()))
        .map_err(|never| match never {})
        .boxed()
}

fn empty_body() -> Body {
    Full::new(Bytes::new())
        .map_err(|never| match never {})
        .boxed()
}

fn decode<T: for<'de> Deserialize<'de>>(metadata: Value, what: &str) -> Result<T> {
    serde_json::from_value(metadata).map_err(|e| api_error(format!("bad {what} from LXD: {e}")))
}

fn api_error(message: impl Into<String>) -> LxdError {
    LxdError::Api {
        status: 500,
        message: message.into(),
    }
}

fn envelope_error(status: StatusCode, envelope: &Value) -> LxdError {
    let code = envelope["error_code"]
        .as_u64()
        .and_then(|code| u16::try_from(code).ok())
        .unwrap_or(status.as_u16());
    LxdError::Api {
        status: code,
        message: envelope["error"]
            .as_str()
            .unwrap_or("unknown LXD error")
            .to_owned(),
    }
}

fn error_from_body(status: StatusCode, body: &[u8]) -> LxdError {
    match serde_json::from_slice::<Value>(body) {
        Ok(envelope) => envelope_error(status, &envelope),
        Err(_) => LxdError::Api {
            status: status.as_u16(),
            message: String::from_utf8_lossy(body).trim().to_owned(),
        },
    }
}

/// Parses one event frame, keeping only instance lifecycle events.
pub fn parse_lifecycle_event(text: &str) -> Option<LifecycleEvent> {
    let event: Value = serde_json::from_str(text).ok()?;
    if event["type"] != "lifecycle" {
        return None;
    }
    let metadata = &event["metadata"];
    let action = metadata["action"].as_str()?.to_owned();
    let source = metadata["source"].as_str().unwrap_or_default();
    let instance = source
        .strip_prefix("/1.0/instances/")
        .map(|rest| rest.split(['?', '/']).next().unwrap_or_default().to_owned())
        .filter(|name| !name.is_empty());
    Some(LifecycleEvent {
        action,
        instance,
        project: event["project"].as_str().unwrap_or("default").to_owned(),
        timestamp: event["timestamp"].as_str().map(str::to_owned),
    })
}

/// Percent-encodes a query value (RFC 3986 unreserved characters pass through).
fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_instance_lifecycle_events() {
        let event = parse_lifecycle_event(
            r#"{"type":"lifecycle","timestamp":"2026-10-22T10:00:00Z","project":"openshell",
                "metadata":{"action":"instance-started","source":"/1.0/instances/openshell-abc?project=openshell"}}"#,
        )
        .unwrap();
        assert_eq!(event.action, "instance-started");
        assert_eq!(event.instance.as_deref(), Some("openshell-abc"));
        assert_eq!(event.project, "openshell");
    }

    #[test]
    fn ignores_non_lifecycle_events() {
        assert!(parse_lifecycle_event(r#"{"type":"logging","metadata":{}}"#).is_none());
    }

    #[test]
    fn scopes_paths_to_project() {
        let client = Client::new("/nonexistent", "my project");
        assert_eq!(
            client.scoped("/1.0/instances?recursion=1"),
            "/1.0/instances?recursion=1&project=my%20project"
        );
        assert_eq!(
            client.scoped("/1.0/images"),
            "/1.0/images?project=my%20project"
        );
    }
}
