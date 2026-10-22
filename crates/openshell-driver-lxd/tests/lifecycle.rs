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
use openshell_driver_lxd::driver::{DriverConfig, ImageSource, LxdDriver};
use openshell_driver_lxd::lxd;
use openshell_driver_lxd::oci::{ImageError, ImportedImage, RuntimeConfig};
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

#[derive(Default)]
struct FakeState {
    instances: HashMap<String, (String, HashMap<String, String>)>,
    created: Vec<Value>,
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
        let body = request.into_body().collect().await.unwrap().to_bytes();
        let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);

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
                    .map(|(name, (status, config))| json!({ "name": name, "status": status, "config": config }))
                    .collect();
                ok(sync(json!(list)))
            }
            ("POST", ["instances"]) => {
                let name = body["name"].as_str().unwrap().to_owned();
                let config: HashMap<String, String> =
                    serde_json::from_value(body["config"].clone()).unwrap();
                let mut state = self.state.lock().unwrap();
                state
                    .instances
                    .insert(name.clone(), ("Stopped".into(), config));
                state.created.push(body);
                drop(state);
                self.emit("instance-created", &name);
                ok(operation)
            }
            ("GET", ["instances", name]) => {
                let state = self.state.lock().unwrap();
                match state.instances.get(*name) {
                    Some((status, config)) => ok(sync(
                        json!({ "name": name, "status": status, "config": config }),
                    )),
                    None => Ok(not_found()),
                }
            }
            ("PATCH", ["instances", name]) => {
                let mut state = self.state.lock().unwrap();
                let Some((_, config)) = state.instances.get_mut(*name) else {
                    return Ok(not_found());
                };
                for (key, value) in body["config"].as_object().unwrap() {
                    config.insert(key.clone(), value.as_str().unwrap().to_owned());
                }
                ok(sync(json!({})))
            }
            ("PUT", ["instances", name, "state"]) => {
                let status = match body["action"].as_str() {
                    Some("start") => "Running",
                    Some("stop") => "Stopped",
                    other => panic!("unexpected action {other:?}"),
                };
                {
                    let mut state = self.state.lock().unwrap();
                    let Some(instance) = state.instances.get_mut(*name) else {
                        return Ok(not_found());
                    };
                    instance.0 = status.into();
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
            ("GET", ["instances", name, "files"]) => {
                if !self.state.lock().unwrap().instances.contains_key(*name) {
                    return Ok(not_found());
                }
                let file = if query.contains("path=/etc/passwd") {
                    PASSWD
                } else if query.contains("path=/etc/group") {
                    GROUP
                } else {
                    return Ok(not_found());
                };
                Ok(Response::new(Full::new(Bytes::from(file))))
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

struct FixedImage;

#[tonic::async_trait]
impl ImageSource for FixedImage {
    async fn ensure(
        &self,
        _lxd: &lxd::Client,
        reference: &str,
    ) -> Result<ImportedImage, ImageError> {
        assert_eq!(reference, "example.com/agent:1");
        Ok(ImportedImage {
            alias: "openshell-oci-abc".into(),
            manifest_digest: "sha256:abc".into(),
            config: RuntimeConfig {
                entrypoint: Some(vec!["/usr/bin/agent".into()]),
                cmd: Some(vec!["--serve".into()]),
                env: Some(vec!["PATH=/usr/bin:/bin".into()]),
                user: Some("agent".into()),
                working_dir: Some("/home/agent".into()),
            },
        })
    }
}

async fn start_driver(dir: &Path) -> (FakeLxd, ComputeDriverClient<Channel>) {
    let lxd_socket = dir.join("lxd.sock");
    let fake = spawn_fake_lxd(&lxd_socket);

    let driver = LxdDriver::new(
        lxd::Client::new(&lxd_socket, "default"),
        Arc::new(FixedImage),
        DriverConfig::default(),
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

fn sandbox(id: &str) -> DriverSandbox {
    DriverSandbox {
        id: id.into(),
        name: "my-agent".into(),
        workspace: "default".into(),
        spec: Some(DriverSandboxSpec {
            environment: HashMap::from([("ANTHROPIC_BASE_URL".into(), "http://proxy".into())]),
            template: Some(DriverSandboxTemplate {
                image: "example.com/agent:1".into(),
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

    {
        let state = fake.state.lock().unwrap();
        let created = &state.created[0];
        assert_eq!(created["name"], "openshell-sbx-1");
        assert_eq!(
            created["profiles"],
            json!([]),
            "sandboxes must not inherit a NIC from profiles"
        );
        assert_eq!(created["devices"]["root"]["pool"], "default");
        assert!(created["devices"].get("eth0").is_none());
        assert_eq!(created["config"]["limits.cpu.allowance"], "50ms/100ms");
        assert_eq!(created["config"]["limits.memory"], "2147483648");
        assert_eq!(created["source"]["alias"], "openshell-oci-abc");

        let (status, config) = &state.instances["openshell-sbx-1"];
        assert_eq!(status, "Running");
        let raw = &config["raw.lxc"];
        assert!(
            raw.contains("lxc.init.cmd = /usr/bin/agent --serve"),
            "{raw}"
        );
        assert!(raw.contains("lxc.init.uid = 1000"));
        assert!(raw.contains("lxc.init.cwd = /home/agent"));
        assert!(raw.contains("lxc.no_new_privs = 1"));
        assert_eq!(config["environment.ANTHROPIC_BASE_URL"], "http://proxy");
        assert_eq!(config["environment.PATH"], "/usr/bin:/bin");
    }

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
    let listed = client
        .list_sandboxes(ListSandboxesRequest {})
        .await
        .unwrap()
        .into_inner()
        .sandboxes;
    assert_eq!(listed.len(), 1);
    assert_eq!(
        listed[0].status.as_ref().unwrap().conditions[0].reason,
        "ContainerStopped"
    );

    client
        .start_sandbox(StartSandboxRequest {
            sandbox_id: "sbx-1".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(
        fake.state.lock().unwrap().instances["openshell-sbx-1"].0,
        "Running"
    );

    let deleted = client
        .delete_sandbox(DeleteSandboxRequest {
            sandbox_id: "sbx-1".into(),
            name: String::new(),
        })
        .await
        .unwrap()
        .into_inner();
    assert!(deleted.deleted);
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
async fn refuses_root_images_and_cleans_up() {
    struct RootImage;
    #[tonic::async_trait]
    impl ImageSource for RootImage {
        async fn ensure(&self, _: &lxd::Client, _: &str) -> Result<ImportedImage, ImageError> {
            Ok(ImportedImage {
                alias: "openshell-oci-root".into(),
                manifest_digest: "sha256:root".into(),
                config: RuntimeConfig {
                    cmd: Some(vec!["/bin/sh".into()]),
                    ..Default::default()
                },
            })
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let lxd_socket = dir.path().join("lxd.sock");
    let fake = spawn_fake_lxd(&lxd_socket);
    let driver = LxdDriver::new(
        lxd::Client::new(&lxd_socket, "default"),
        Arc::new(RootImage),
        DriverConfig::default(),
    );
    let error = driver.create(&sandbox("sbx-root")).await.unwrap_err();
    assert!(error.to_string().contains("root"), "{error}");
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
