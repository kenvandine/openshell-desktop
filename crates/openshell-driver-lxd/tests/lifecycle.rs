//! Drives the gRPC service end to end against a fake LXD daemon on a
//! temporary Unix socket, the way the gateway would.

use std::collections::HashMap;
use std::convert::Infallible;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use openshell_core::jwt::{
    CredentialEpoch, SandboxLaunchAuthentication, SecretJwt, SessionRotation,
    SessionVerificationKey, SupervisorAuthBundle,
};
use openshell_driver_lxd::driver::{DriverConfig, ImageSource, LxdDriver};
use openshell_driver_lxd::lxd;
use openshell_driver_lxd::oci::{ExtractedFile, ImageError, ImportedImage, RuntimeConfig};
use openshell_driver_lxd::proto::compute::v1::compute_driver_client::ComputeDriverClient;
use openshell_driver_lxd::proto::compute::v1::compute_driver_server::ComputeDriverServer;
use openshell_driver_lxd::proto::compute::v1::{
    CreateSandboxRequest, DeleteSandboxRequest, DriverResourceRequirements, DriverSandbox,
    DriverSandboxSpec, DriverSandboxTemplate, GetCapabilitiesRequest, GetSandboxRequest,
    ListSandboxesRequest, StartSandboxRequest, StopSandboxRequest, WatchSandboxesRequest,
    watch_sandboxes_event,
};
use openshell_driver_lxd::proto::extension::v1::{PeerMetadata, ProtocolVersion};
use openshell_driver_lxd::service::Service;
use serde_json::{Value, json};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::broadcast;
use tokio_stream::wrappers::UnixListenerStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::Role;
use tonic::transport::{Channel, Endpoint};

const PASSWD: &str = "root:x:0:0::/root:/bin/sh\nagent:x:1000:1000::/home/agent:/bin/sh\n";
const GROUP: &str = "root:x:0:\nagent:x:1000:\n";

/// One file or directory pushed through the file API.
#[derive(Clone, Debug)]
struct Pushed {
    content: Vec<u8>,
    kind: String,
    uid: String,
    gid: String,
    mode: String,
}

struct FakeInstance {
    status: String,
    config: HashMap<String, String>,
    devices: Value,
    files: HashMap<String, Pushed>,
}

#[derive(Default)]
struct FakeState {
    instances: HashMap<String, FakeInstance>,
    volumes: Vec<String>,
    created: Vec<Value>,
    /// Every state change, in order, as `(instance, action)`.
    actions: Vec<(String, String)>,
}

impl FakeState {
    fn file(&self, instance: &str, path: &str) -> Option<&Pushed> {
        self.instances.get(instance)?.files.get(path)
    }
}

#[derive(Clone)]
struct FakeLxd {
    state: Arc<Mutex<FakeState>>,
    events: broadcast::Sender<String>,
}

impl FakeLxd {
    fn emit(&self, action: &str, name: &str) {
        let event = json!({
            "type": "lifecycle",
            "project": "default",
            "metadata": { "action": action, "source": format!("/1.0/instances/{name}") },
        });
        let _ = self.events.send(event.to_string());
    }

    async fn handle(self, request: Request<Incoming>) -> Result<Response<Full<Bytes>>, Infallible> {
        let method = request.method().clone();
        let uri = request.uri().clone();
        let path = uri.path().to_owned();
        let query = uri.query().unwrap_or_default().to_owned();

        if path == "/1.0/events" {
            return Ok(self.upgrade_events(request));
        }
        let headers = request.headers().clone();
        let raw = request.into_body().collect().await.unwrap().to_bytes();
        let body: Value = serde_json::from_slice(&raw).unwrap_or(Value::Null);

        let sync =
            |metadata: Value| json!({ "type": "sync", "status_code": 200, "metadata": metadata });
        let operation =
            json!({ "type": "async", "status_code": 100, "metadata": { "id": "op-1" } });
        let not_found = || {
            Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(Full::new(Bytes::from(
                    json!({ "type": "error", "error": "Not Found", "error_code": 404 }).to_string(),
                )))
                .unwrap()
        };
        let ok = |value: Value| Ok(Response::new(Full::new(Bytes::from(value.to_string()))));

        let segments: Vec<&str> = path
            .trim_start_matches("/1.0")
            .split('/')
            .filter(|s| !s.is_empty())
            .collect();
        match (method.as_str(), segments.as_slice()) {
            ("GET", []) => ok(sync(json!({ "environment": { "server_version": "6.5" } }))),
            ("GET", ["operations", _, "wait"]) => {
                ok(sync(json!({ "status": "Success", "metadata": {} })))
            }
            ("GET", ["instances"]) => {
                let state = self.state.lock().unwrap();
                let list: Vec<Value> = state
                    .instances
                    .iter()
                    .map(|(name, i)| instance_json(name, i))
                    .collect();
                ok(sync(json!(list)))
            }
            ("POST", ["instances"]) => {
                let name = body["name"].as_str().unwrap().to_owned();
                let config: HashMap<String, String> =
                    serde_json::from_value(body["config"].clone()).unwrap();
                let mut state = self.state.lock().unwrap();
                state.instances.insert(
                    name.clone(),
                    FakeInstance {
                        status: "Stopped".into(),
                        config,
                        devices: body["devices"].clone(),
                        files: HashMap::new(),
                    },
                );
                state.created.push(body);
                drop(state);
                self.emit("instance-created", &name);
                ok(operation)
            }
            ("GET", ["instances", name]) => {
                let state = self.state.lock().unwrap();
                match state.instances.get(*name) {
                    Some(instance) => ok(sync(instance_json(name, instance))),
                    None => Ok(not_found()),
                }
            }
            ("PATCH", ["instances", name]) => {
                let mut state = self.state.lock().unwrap();
                let Some(instance) = state.instances.get_mut(*name) else {
                    return Ok(not_found());
                };
                for (key, value) in body["config"].as_object().unwrap() {
                    instance
                        .config
                        .insert(key.clone(), value.as_str().unwrap().to_owned());
                }
                ok(sync(json!({})))
            }
            ("PUT", ["instances", name, "state"]) => {
                let action = body["action"].as_str().unwrap().to_owned();
                let status = match action.as_str() {
                    "start" => "Running",
                    "stop" => "Stopped",
                    other => panic!("unexpected action {other:?}"),
                };
                {
                    let mut state = self.state.lock().unwrap();
                    let Some(instance) = state.instances.get_mut(*name) else {
                        return Ok(not_found());
                    };
                    instance.status = status.into();
                    state.actions.push(((*name).to_owned(), action));
                }
                self.emit(
                    if status == "Running" {
                        "instance-started"
                    } else {
                        "instance-stopped"
                    },
                    name,
                );
                ok(operation)
            }
            ("DELETE", ["instances", name]) => {
                if self.state.lock().unwrap().instances.remove(*name).is_none() {
                    return Ok(not_found());
                }
                self.emit("instance-deleted", name);
                ok(operation)
            }
            ("POST", ["instances", name, "files"]) => {
                let pushed_path = query_param(&query, "path");
                let header = |key: &str| {
                    headers
                        .get(key)
                        .map(|v| v.to_str().unwrap().to_owned())
                        .unwrap_or_default()
                };
                let mut state = self.state.lock().unwrap();
                let Some(instance) = state.instances.get_mut(*name) else {
                    return Ok(not_found());
                };
                instance.files.insert(
                    pushed_path,
                    Pushed {
                        content: raw.to_vec(),
                        kind: header("x-lxd-type"),
                        uid: header("x-lxd-uid"),
                        gid: header("x-lxd-gid"),
                        mode: header("x-lxd-mode"),
                    },
                );
                ok(sync(json!({})))
            }
            ("GET", ["instances", name, "files"]) => {
                let state = self.state.lock().unwrap();
                let Some(instance) = state.instances.get(*name) else {
                    return Ok(not_found());
                };
                let wanted = query_param(&query, "path");
                if let Some(pushed) = instance.files.get(&wanted) {
                    return Ok(Response::new(Full::new(Bytes::from(
                        pushed.content.clone(),
                    ))));
                }
                let file = match wanted.as_str() {
                    "/etc/passwd" => PASSWD,
                    "/etc/group" => GROUP,
                    _ => return Ok(not_found()),
                };
                Ok(Response::new(Full::new(Bytes::from(file))))
            }
            ("POST", ["storage-pools", _, "volumes", "custom"]) => {
                self.state
                    .lock()
                    .unwrap()
                    .volumes
                    .push(body["name"].as_str().unwrap().to_owned());
                ok(sync(json!({})))
            }
            ("DELETE", ["storage-pools", _, "volumes", "custom", volume]) => {
                let mut state = self.state.lock().unwrap();
                let before = state.volumes.len();
                state.volumes.retain(|v| v != volume);
                if state.volumes.len() == before {
                    return Ok(not_found());
                }
                ok(sync(json!({})))
            }
            _ => panic!("unexpected request {method} {path}"),
        }
    }

    fn upgrade_events(self, mut request: Request<Incoming>) -> Response<Full<Bytes>> {
        let key = request.headers()["sec-websocket-key"].as_bytes().to_owned();
        let accept = tokio_tungstenite::tungstenite::handshake::derive_accept_key(&key);
        let mut events = self.events.subscribe();
        tokio::spawn(async move {
            let upgraded = hyper::upgrade::on(&mut request).await.unwrap();
            let mut socket = tokio_tungstenite::WebSocketStream::from_raw_socket(
                TokioIo::new(upgraded),
                Role::Server,
                None,
            )
            .await;
            while let Ok(event) = events.recv().await {
                if socket.send(Message::text(event)).await.is_err() {
                    return;
                }
            }
        });
        Response::builder()
            .status(StatusCode::SWITCHING_PROTOCOLS)
            .header("Connection", "Upgrade")
            .header("Upgrade", "websocket")
            .header("Sec-WebSocket-Accept", accept)
            .body(Full::new(Bytes::new()))
            .unwrap()
    }
}

fn instance_json(name: &str, instance: &FakeInstance) -> Value {
    json!({
        "name": name,
        "status": instance.status,
        "config": instance.config,
        "devices": instance.devices,
        "expanded_devices": instance.devices,
        "profiles": [],
    })
}

fn query_param(query: &str, key: &str) -> String {
    query
        .split('&')
        .find_map(|pair| pair.strip_prefix(&format!("{key}=")))
        .unwrap_or_default()
        .to_owned()
}

fn spawn_fake_lxd(path: &Path) -> FakeLxd {
    let (events, _) = broadcast::channel(64);
    let fake = FakeLxd {
        state: Arc::default(),
        events,
    };
    let listener = UnixListener::bind(path).unwrap();
    let server = fake.clone();
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let fake = server.clone();
            tokio::spawn(async move {
                let service =
                    hyper::service::service_fn(move |request| fake.clone().handle(request));
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .with_upgrades()
                    .await;
            });
        }
    });
    fake
}

const WORKLOAD_IMAGE: &str = "example.com/agent:1";
const RUNTIME_IMAGE: &str = "example.com/runtime:1";
const SUPERVISOR_IMAGE: &str = "example.com/supervisor:1";

struct FixedImage {
    user: Option<&'static str>,
}

#[tonic::async_trait]
impl ImageSource for FixedImage {
    async fn ensure(
        &self,
        _lxd: &lxd::Client,
        reference: &str,
    ) -> Result<ImportedImage, ImageError> {
        Ok(match reference {
            WORKLOAD_IMAGE => ImportedImage {
                alias: "openshell-oci-abc".into(),
                manifest_digest: "sha256:abc".into(),
                config: RuntimeConfig {
                    entrypoint: Some(vec!["/usr/bin/agent".into()]),
                    cmd: Some(vec!["--serve".into()]),
                    env: Some(vec!["PATH=/usr/bin:/bin".into(), "LANG=C".into()]),
                    user: self.user.map(str::to_owned),
                    working_dir: Some("/home/agent".into()),
                },
            },
            SUPERVISOR_IMAGE => ImportedImage {
                alias: "openshell-oci-sup".into(),
                manifest_digest: "sha256:sup".into(),
                config: RuntimeConfig::default(),
            },
            other => panic!("unexpected image {other}"),
        })
    }

    async fn extract_file(&self, reference: &str, path: &str) -> Result<ExtractedFile, ImageError> {
        assert_eq!((reference, path), (RUNTIME_IMAGE, "/openshell-sandbox"));
        Ok(ExtractedFile {
            manifest_digest: "sha256:rt".into(),
            content: b"\x7fELF-fake-runtime".to_vec(),
        })
    }
}

fn driver_config() -> DriverConfig {
    DriverConfig {
        grpc_endpoint: "https://127.0.0.1:8080".into(),
        sandbox_runtime_image: RUNTIME_IMAGE.into(),
        supervisor_image: SUPERVISOR_IMAGE.into(),
        resolv_conf: Some(b"nameserver 127.0.0.53\n".to_vec()),
        ..DriverConfig::default()
    }
}

async fn start_driver(dir: &Path) -> (FakeLxd, ComputeDriverClient<Channel>) {
    let lxd_socket = dir.join("lxd.sock");
    let fake = spawn_fake_lxd(&lxd_socket);

    let driver = LxdDriver::new(
        lxd::Client::new(&lxd_socket, "default"),
        Arc::new(FixedImage {
            user: Some("agent"),
        }),
        driver_config(),
    );
    tokio::spawn(driver.clone().pump_events());

    let driver_socket = dir.join("driver.sock");
    let listener = UnixListener::bind(&driver_socket).unwrap();
    tokio::spawn(
        tonic::transport::Server::builder()
            .add_service(ComputeDriverServer::new(Service::new(driver)))
            .serve_with_incoming(UnixListenerStream::new(listener)),
    );

    let path: PathBuf = driver_socket;
    let channel = Endpoint::try_from("http://driver.invalid")
        .unwrap()
        .connect_with_connector(tower::service_fn(move |_| {
            let path = path.clone();
            async move { UnixStream::connect(path).await.map(TokioIo::new) }
        }))
        .await
        .unwrap();
    (fake, ComputeDriverClient::new(channel))
}

fn launch_authentication(generation: &str) -> Vec<u8> {
    serde_json::to_vec(&SandboxLaunchAuthentication {
        supervisor: SupervisorAuthBundle {
            session_id: openshell_core::SandboxSessionId::new(),
            runtime_generation: openshell_core::sandbox_generation::SandboxGenerationId::parse(
                generation,
            )
            .unwrap(),
            session_rotation: SessionRotation::new(1).unwrap(),
            auth_epoch: CredentialEpoch::new(1).unwrap(),
            gateway_token: SecretJwt::parse("gateway.token.value").unwrap(),
            gateway_expires_at: i64::MAX,
            sandbox_token: SecretJwt::parse("sandbox.token.value").unwrap(),
            sandbox_expires_at: i64::MAX,
            ssh_host_private_key: None,
            ocsf_otlp_headers: None,
        },
        gateway_id: "gateway-test".into(),
        verification_keys: vec![SessionVerificationKey {
            key_id: "test-key".into(),
            public_key_pem: b"public-key".to_vec(),
        }],
    })
    .unwrap()
}

fn sandbox(id: &str) -> DriverSandbox {
    DriverSandbox {
        id: id.into(),
        name: "my-agent".into(),
        workspace: "default".into(),
        spec: Some(DriverSandboxSpec {
            environment: HashMap::from([("ANTHROPIC_BASE_URL".into(), "http://proxy".into())]),
            launch_authentication: launch_authentication("generation-1"),
            template: Some(DriverSandboxTemplate {
                image: WORKLOAD_IMAGE.into(),
                resources: Some(DriverResourceRequirements {
                    cpu_limit: "500m".into(),
                    memory_limit: "2Gi".into(),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn gateway() -> PeerMetadata {
    PeerMetadata {
        protocol_version: Some(ProtocolVersion { major: 1, minor: 0 }),
        implementation_name: "openshell/gateway".into(),
        implementation_version: "test".into(),
        supported_capabilities: vec!["openshell.compute.contract".into()],
        required_capabilities: vec!["openshell.compute.contract".into()],
    }
}

async fn eventually(what: &str, mut check: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !check() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}

#[tokio::test]
async fn negotiates_capabilities() {
    let dir = tempfile::tempdir().unwrap();
    let (_fake, mut client) = start_driver(dir.path()).await;
    let capabilities = client
        .get_capabilities(GetCapabilitiesRequest {
            gateway: Some(gateway()),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(capabilities.driver_name, "lxd");
    let extension = capabilities.extension.unwrap();
    assert_eq!(extension.protocol_version.unwrap().major, 1);
    assert!(
        extension
            .supported_capabilities
            .contains(&"openshell.compute.contract".to_string())
    );
    assert!(capabilities.resource_admission_policy.starts_with("v1:{"));

    let refused = client
        .get_capabilities(GetCapabilitiesRequest { gateway: None })
        .await
        .unwrap_err();
    assert_eq!(refused.code(), tonic::Code::FailedPrecondition);
}

#[tokio::test]
async fn provisions_a_workload_and_supervisor_pair() {
    let dir = tempfile::tempdir().unwrap();
    let (fake, mut client) = start_driver(dir.path()).await;
    client
        .create_sandbox(CreateSandboxRequest {
            sandbox: Some(sandbox("sbx-1")),
        })
        .await
        .unwrap();

    let state = fake.state.lock().unwrap();
    assert_eq!(state.volumes, vec!["openshell-control-sbx-1"]);

    let workload_body = state
        .created
        .iter()
        .find(|b| b["name"] == "openshell-sbx-1")
        .unwrap();
    assert_eq!(
        workload_body["profiles"],
        json!([]),
        "the workload must not inherit a NIC from profiles"
    );
    assert_eq!(workload_body["devices"]["root"]["pool"], "default");
    assert_eq!(
        workload_body["devices"]["control"]["source"],
        "openshell-control-sbx-1"
    );
    assert!(
        workload_body["devices"]
            .as_object()
            .unwrap()
            .values()
            .all(|d| d["type"] != "nic")
    );
    assert_eq!(
        workload_body["config"]["limits.cpu.allowance"],
        "50ms/100ms"
    );
    assert_eq!(workload_body["config"]["limits.memory"], "2147483648");
    assert_eq!(workload_body["source"]["alias"], "openshell-oci-abc");
    let supervisor_body = state
        .created
        .iter()
        .find(|b| b["name"] == "openshell-sup-sbx-1")
        .unwrap();
    assert_eq!(supervisor_body["source"]["alias"], "openshell-oci-sup");
    assert_eq!(
        supervisor_body["devices"]["control"]["source"],
        "openshell-control-sbx-1"
    );

    // Workload: runtime as init under the resolved identity, no agent env.
    let workload = &state.instances["openshell-sbx-1"];
    assert_eq!(workload.status, "Running");
    let raw = &workload.config["raw.lxc"];
    assert!(
        raw.contains(
            "lxc.init.cmd = /opt/openshell/bin/openshell-sandbox --bootstrap /.openshell/channel/sandbox/bootstrap.json"
        ),
        "{raw}"
    );
    assert!(raw.contains("lxc.init.uid = 1000") && raw.contains("lxc.no_new_privs = 1"));
    assert!(!raw.contains("lxc.net"), "{raw}");
    assert!(
        !workload
            .config
            .keys()
            .any(|k| k.starts_with("environment.") && k != "environment.PATH"),
        "agent environment must not reach the privileged runtime"
    );
    let runtime = state
        .file("openshell-sbx-1", "/opt/openshell/bin/openshell-sandbox")
        .unwrap();
    assert_eq!((runtime.mode.as_str(), runtime.uid.as_str()), ("0555", "0"));
    assert!(runtime.content.starts_with(b"\x7fELF"));
    let bootstrap = state
        .file(
            "openshell-sbx-1",
            "/.openshell/channel/sandbox/bootstrap.json",
        )
        .unwrap();
    assert_eq!(
        (
            bootstrap.uid.as_str(),
            bootstrap.gid.as_str(),
            bootstrap.mode.as_str()
        ),
        ("1000", "1000", "0600")
    );
    let config: Value = serde_json::from_slice(&bootstrap.content).unwrap();
    assert_eq!(config["boundary_id"], "sbx-1");
    assert_eq!(config["child_env"]["LANG"], "C");
    assert_eq!(config["child_env"]["ANTHROPIC_BASE_URL"], "http://proxy");
    assert!(
        state
            .file("openshell-sbx-1", "/.openshell/channel/sandbox/server.key")
            .is_some()
    );
    // The control mount belongs to the workload identity.
    let control = state
        .file("openshell-sbx-1", "/.openshell/channel/control")
        .unwrap();
    assert_eq!(
        (
            control.kind.as_str(),
            control.uid.as_str(),
            control.mode.as_str()
        ),
        ("directory", "1000", "0700")
    );

    // Supervisor: its own credentials, never the workload's.
    let supervisor = &state.instances["openshell-sup-sbx-1"];
    assert_eq!(supervisor.status, "Running");
    let raw = &supervisor.config["raw.lxc"];
    assert!(
        raw.contains("lxc.init.cmd = /openshell-supervisor --workdir /home/agent"),
        "{raw}"
    );
    assert!(
        raw.contains("--backend-descriptor-file=/.openshell/supervisor/runtime-descriptor.json")
    );
    assert!(
        raw.contains("lxc.net.0.type = none"),
        "host network by default: {raw}"
    );
    assert_eq!(
        supervisor.config["environment.OPENSHELL_SANDBOX_ID"],
        "sbx-1"
    );
    assert_eq!(
        supervisor.config["environment.OPENSHELL_ENDPOINT"],
        "https://127.0.0.1:8080"
    );
    assert!(supervisor.config["environment.OPENSHELL_MAIN_PROCESS_SPEC"].contains("\"version\":1"));
    assert!(
        supervisor.config["environment.OPENSHELL_USER_ENVIRONMENT"].contains("ANTHROPIC_BASE_URL")
    );
    assert_eq!(
        supervisor.config["environment.OPENSHELL_OCI_IMAGE_USER"],
        "agent"
    );
    for path in [
        "/.openshell/supervisor/runtime-descriptor.json",
        "/.openshell/supervisor/auth.json",
        "/.openshell/supervisor/restart-metadata.json",
    ] {
        assert!(state.file("openshell-sup-sbx-1", path).is_some(), "{path}");
        assert!(
            state.file("openshell-sbx-1", path).is_none(),
            "{path} leaked"
        );
    }
    assert!(
        state
            .file("openshell-sup-sbx-1", "/etc/resolv.conf")
            .is_some()
    );
    assert!(
        state
            .file("openshell-sbx-1", "/.openshell/supervisor/auth.json")
            .is_none()
    );

    // Workload first, supervisor second.
    let starts: Vec<&str> = state
        .actions
        .iter()
        .filter(|(_, action)| action == "start")
        .map(|(name, _)| name.as_str())
        .collect();
    assert_eq!(starts, vec!["openshell-sbx-1", "openshell-sup-sbx-1"]);
}

#[tokio::test]
async fn runs_a_sandbox_through_its_lifecycle() {
    let dir = tempfile::tempdir().unwrap();
    let (fake, mut client) = start_driver(dir.path()).await;
    let mut watch = client
        .watch_sandboxes(WatchSandboxesRequest {})
        .await
        .unwrap()
        .into_inner();

    client
        .create_sandbox(CreateSandboxRequest {
            sandbox: Some(sandbox("sbx-1")),
        })
        .await
        .unwrap();

    let got = client
        .get_sandbox(GetSandboxRequest {
            sandbox_id: "sbx-1".into(),
            name: String::new(),
        })
        .await
        .unwrap()
        .into_inner()
        .sandbox
        .unwrap();
    let status = got.status.unwrap();
    assert_eq!(got.name, "my-agent");
    assert_eq!(status.conditions[0].status, "True");
    assert_eq!(status.resolved_identity.unwrap().uid, 1000);

    client
        .stop_sandbox(StopSandboxRequest {
            sandbox_id: "sbx-1".into(),
            name: String::new(),
        })
        .await
        .unwrap();
    {
        let state = fake.state.lock().unwrap();
        assert_eq!(state.instances["openshell-sbx-1"].status, "Stopped");
        assert_eq!(state.instances["openshell-sup-sbx-1"].status, "Stopped");
        let stops: Vec<&str> = state
            .actions
            .iter()
            .filter(|(_, action)| action == "stop")
            .map(|(name, _)| name.as_str())
            .collect();
        assert_eq!(stops, vec!["openshell-sup-sbx-1", "openshell-sbx-1"]);
    }
    let listed = client
        .list_sandboxes(ListSandboxesRequest {})
        .await
        .unwrap()
        .into_inner()
        .sandboxes;
    assert_eq!(listed.len(), 1, "the supervisor is not a sandbox");
    assert_eq!(
        listed[0].status.as_ref().unwrap().conditions[0].reason,
        "ContainerStopped"
    );

    // Starting needs fresh launch credentials and a generation.
    let refused = client
        .start_sandbox(StartSandboxRequest {
            sandbox_id: "sbx-1".into(),
            generation_id: "generation-2".into(),
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert_eq!(refused.code(), tonic::Code::FailedPrecondition);
    client
        .start_sandbox(StartSandboxRequest {
            sandbox_id: "sbx-1".into(),
            generation_id: "generation-2".into(),
            launch_authentication: launch_authentication("generation-2"),
            ..Default::default()
        })
        .await
        .unwrap();
    {
        let state = fake.state.lock().unwrap();
        assert_eq!(state.instances["openshell-sbx-1"].status, "Running");
        assert_eq!(state.instances["openshell-sup-sbx-1"].status, "Running");
        // The restart rewrote the bootstrap for the new generation.
        let metadata = state
            .file(
                "openshell-sup-sbx-1",
                "/.openshell/supervisor/restart-metadata.json",
            )
            .unwrap();
        let metadata: Value = serde_json::from_slice(&metadata.content).unwrap();
        assert_ne!(metadata["generation"], "");
        let bootstrap = state
            .file(
                "openshell-sbx-1",
                "/.openshell/channel/sandbox/bootstrap.json",
            )
            .unwrap();
        let bootstrap: Value = serde_json::from_slice(&bootstrap.content).unwrap();
        assert_eq!(bootstrap["generation"], "generation-2");
    }
    // Credentials are always required, even for a retry.
    client
        .start_sandbox(StartSandboxRequest {
            sandbox_id: "sbx-1".into(),
            generation_id: "generation-2".into(),
            ..Default::default()
        })
        .await
        .unwrap_err();

    let deleted = client
        .delete_sandbox(DeleteSandboxRequest {
            sandbox_id: "sbx-1".into(),
            name: String::new(),
        })
        .await
        .unwrap()
        .into_inner();
    assert!(deleted.deleted);
    {
        let state = fake.state.lock().unwrap();
        assert!(state.instances.is_empty());
        assert!(state.volumes.is_empty());
    }
    let again = client
        .delete_sandbox(DeleteSandboxRequest {
            sandbox_id: "sbx-1".into(),
            name: String::new(),
        })
        .await
        .unwrap()
        .into_inner();
    assert!(!again.deleted);

    // The watch stream ends with the deletion, reported by sandbox ID.
    let deleted_id = tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(event) = watch.next().await {
            if let Some(watch_sandboxes_event::Payload::Deleted(deleted)) = event.unwrap().payload {
                return deleted.sandbox_id;
            }
        }
        panic!("watch stream ended");
    })
    .await
    .expect("no deletion event");
    assert_eq!(deleted_id, "sbx-1");
}

#[tokio::test]
async fn stops_the_workload_when_its_supervisor_dies() {
    let dir = tempfile::tempdir().unwrap();
    let (fake, mut client) = start_driver(dir.path()).await;
    client
        .create_sandbox(CreateSandboxRequest {
            sandbox: Some(sandbox("sbx-1")),
        })
        .await
        .unwrap();

    // The supervisor exits on its own; nothing enforces policy any more.
    fake.state
        .lock()
        .unwrap()
        .instances
        .get_mut("openshell-sup-sbx-1")
        .unwrap()
        .status = "Stopped".into();
    fake.emit("instance-stopped", "openshell-sup-sbx-1");

    eventually("the workload to be stopped", || {
        fake.state.lock().unwrap().instances["openshell-sbx-1"].status == "Stopped"
    })
    .await;
    let got = client
        .get_sandbox(GetSandboxRequest {
            sandbox_id: "sbx-1".into(),
            name: String::new(),
        })
        .await
        .unwrap()
        .into_inner()
        .sandbox
        .unwrap();
    let condition = &got.status.unwrap().conditions[0];
    assert_eq!(condition.status, "False");
    assert_eq!(condition.reason, "ContainerExited");
}

#[tokio::test]
async fn refuses_root_images_and_cleans_up() {
    let dir = tempfile::tempdir().unwrap();
    let lxd_socket = dir.path().join("lxd.sock");
    let fake = spawn_fake_lxd(&lxd_socket);
    let driver = LxdDriver::new(
        lxd::Client::new(&lxd_socket, "default"),
        Arc::new(FixedImage { user: Some("root") }),
        driver_config(),
    );
    let error = driver.create(&sandbox("sbx-root")).await.unwrap_err();
    assert!(error.to_string().contains("zero"), "{error}");
    let state = fake.state.lock().unwrap();
    assert!(state.instances.is_empty(), "failed creates must clean up");
    assert!(state.volumes.is_empty());
}

#[tokio::test]
async fn requires_launch_authentication_and_a_gateway_endpoint() {
    let dir = tempfile::tempdir().unwrap();
    let lxd_socket = dir.path().join("lxd.sock");
    let fake = spawn_fake_lxd(&lxd_socket);
    let driver = LxdDriver::new(
        lxd::Client::new(&lxd_socket, "default"),
        Arc::new(FixedImage {
            user: Some("agent"),
        }),
        driver_config(),
    );
    let mut anonymous = sandbox("sbx-anon");
    anonymous
        .spec
        .as_mut()
        .unwrap()
        .launch_authentication
        .clear();
    let error = driver.create(&anonymous).await.unwrap_err();
    assert!(
        error.to_string().contains("launch authentication"),
        "{error}"
    );

    let driver = LxdDriver::new(
        lxd::Client::new(&lxd_socket, "default"),
        Arc::new(FixedImage {
            user: Some("agent"),
        }),
        DriverConfig {
            grpc_endpoint: String::new(),
            ..driver_config()
        },
    );
    let error = driver.create(&sandbox("sbx-noep")).await.unwrap_err();
    assert!(error.to_string().contains("--grpc-endpoint"), "{error}");
    assert!(fake.state.lock().unwrap().instances.is_empty());
}

#[tokio::test]
async fn validates_requests_before_touching_lxd() {
    let dir = tempfile::tempdir().unwrap();
    let (_fake, mut client) = start_driver(dir.path()).await;

    let mut bad = sandbox("sbx-2");
    bad.spec
        .as_mut()
        .unwrap()
        .template
        .as_mut()
        .unwrap()
        .resources = Some(DriverResourceRequirements {
        memory_limit: "lots".into(),
        ..Default::default()
    });
    let error = client
        .validate_sandbox_create(
            openshell_driver_lxd::proto::compute::v1::ValidateSandboxCreateRequest {
                sandbox: Some(bad),
            },
        )
        .await
        .unwrap_err();
    assert_eq!(error.code(), tonic::Code::InvalidArgument);

    let mut gpu = sandbox("sbx-3");
    gpu.spec.as_mut().unwrap().resource_requirements = Some(
        openshell_driver_lxd::proto::compute::v1::ResourceRequirements {
            gpu: Some(Default::default()),
        },
    );
    let error = client
        .create_sandbox(CreateSandboxRequest { sandbox: Some(gpu) })
        .await
        .unwrap_err();
    assert_eq!(error.code(), tonic::Code::FailedPrecondition);
}
