//! Client for the OpenShell gateway's user-facing gRPC API.
//!
//! The desktop app talks to the gateway bundled in the snap over mTLS, using
//! the client certificate `openshell-gateway generate-certs` wrote next to the
//! gateway's state. Everything here is async; the app runs it on its own
//! runtime thread.

use std::path::{Path, PathBuf};
use std::time::Duration;

use openshell_core::proto::datamodel::v1::{WorkspaceSelector, workspace_selector::Selection};
use openshell_core::proto::{
    CreateSandboxRequest, DeleteSandboxRequest, ListSandboxesRequest, Sandbox, SandboxPhase,
    SandboxSpec, StartSandboxRequest, StopSandboxRequest, open_shell_client::OpenShellClient,
};
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Endpoint, Identity};

pub use openshell_core::proto;
pub use tonic::{Code, Status};

/// Port the snap's gateway listens on unless `gateway-port` is set.
pub const DEFAULT_PORT: u16 = 17671;
/// OpenShell scopes sandboxes to workspaces; the app uses the default one.
pub const WORKSPACE: &str = "default";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("gateway TLS material not found in {0}; is the gateway service enabled?")]
    NoTlsMaterial(PathBuf),
    #[error("reading {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("gateway unreachable at {endpoint}: {source}")]
    Connect {
        endpoint: String,
        source: tonic::transport::Error,
    },
    #[error("invalid gateway endpoint: {0}")]
    Endpoint(tonic::transport::Error),
    #[error("{}", .0.message())]
    Rpc(#[from] tonic::Status),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub endpoint: String,
    /// Directory holding `ca.crt` and `client/tls.{crt,key}`.
    pub tls_dir: PathBuf,
}

impl Config {
    /// The snap's gateway: `OPENSHELL_LOCAL_TLS_DIR` when set (inside the snap
    /// it points at `$SNAP_USER_COMMON/gateway/tls`), otherwise the snap's
    /// per-user data in the home directory.
    pub fn discover() -> Self {
        let tls_dir = std::env::var_os("OPENSHELL_LOCAL_TLS_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                let home = std::env::var_os("HOME")
                    .map(PathBuf::from)
                    .unwrap_or_default();
                home.join("snap/openshell-desktop/common/gateway/tls")
            });
        let port = std::env::var("OPENSHELL_GATEWAY_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(DEFAULT_PORT);
        Self {
            endpoint: format!("https://127.0.0.1:{port}"),
            tls_dir,
        }
    }
}

fn read(path: &Path) -> Result<Vec<u8>, Error> {
    std::fs::read(path).map_err(|source| Error::Read {
        path: path.into(),
        source,
    })
}

fn workspace() -> Option<WorkspaceSelector> {
    Some(WorkspaceSelector {
        selection: Some(Selection::Workspace(WORKSPACE.into())),
    })
}

/// How the app shows a sandbox's phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Running,
    Stopped,
    Transitioning,
    Failed,
}

impl State {
    pub fn from_phase(phase: SandboxPhase) -> Self {
        match phase {
            SandboxPhase::Ready => State::Running,
            SandboxPhase::Stopped | SandboxPhase::Completed => State::Stopped,
            SandboxPhase::Error => State::Failed,
            SandboxPhase::Provisioning
            | SandboxPhase::Deleting
            | SandboxPhase::Stopping
            | SandboxPhase::Starting
            | SandboxPhase::Unspecified
            | SandboxPhase::Unknown => State::Transitioning,
        }
    }
}

/// The parts of a gateway sandbox the app displays.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxInfo {
    pub id: String,
    pub name: String,
    pub state: State,
    pub phase: String,
    pub image: String,
}

impl SandboxInfo {
    pub fn from_proto(sandbox: &Sandbox) -> Self {
        let meta = sandbox.metadata.as_ref();
        let phase = sandbox
            .status
            .as_ref()
            .map_or(SandboxPhase::Unspecified, |s| s.phase());
        Self {
            id: meta.map(|m| m.id.clone()).unwrap_or_default(),
            name: meta.map(|m| m.name.clone()).unwrap_or_default(),
            state: State::from_phase(phase),
            phase: phase
                .as_str_name()
                .trim_start_matches("SANDBOX_PHASE_")
                .to_lowercase(),
            image: sandbox
                .spec
                .as_ref()
                .and_then(|s| s.template.as_ref())
                .map(|t| t.image.clone())
                .unwrap_or_default(),
        }
    }
}

#[derive(Clone)]
pub struct Gateway {
    client: OpenShellClient<Channel>,
}

impl Gateway {
    pub async fn connect(config: &Config) -> Result<Self, Error> {
        let ca = config.tls_dir.join("ca.crt");
        if !ca.exists() {
            return Err(Error::NoTlsMaterial(config.tls_dir.clone()));
        }
        let identity = Identity::from_pem(
            read(&config.tls_dir.join("client/tls.crt"))?,
            read(&config.tls_dir.join("client/tls.key"))?,
        );
        let tls = ClientTlsConfig::new()
            .ca_certificate(Certificate::from_pem(read(&ca)?))
            .identity(identity)
            .domain_name("localhost");
        let channel = Endpoint::from_shared(config.endpoint.clone())
            .map_err(Error::Endpoint)?
            .tls_config(tls)
            .map_err(Error::Endpoint)?
            .connect_timeout(Duration::from_secs(3))
            .http2_keep_alive_interval(Duration::from_secs(10))
            .keep_alive_while_idle(true)
            .connect()
            .await
            .map_err(|source| Error::Connect {
                endpoint: config.endpoint.clone(),
                source,
            })?;
        Ok(Self {
            client: OpenShellClient::new(channel),
        })
    }

    pub async fn list(&mut self) -> Result<Vec<SandboxInfo>, Error> {
        let response = self
            .client
            .list_sandboxes(ListSandboxesRequest {
                workspace_scope: workspace(),
                ..Default::default()
            })
            .await?
            .into_inner();
        Ok(response
            .sandboxes
            .iter()
            .map(SandboxInfo::from_proto)
            .collect())
    }

    pub async fn create(&mut self, name: &str, image: &str) -> Result<SandboxInfo, Error> {
        // An empty image leaves the choice to the gateway's default.
        let spec = SandboxSpec {
            template: (!image.is_empty()).then(|| openshell_core::proto::SandboxTemplate {
                image: image.into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        let response = self
            .client
            .create_sandbox(CreateSandboxRequest {
                workspace_scope: workspace(),
                name: name.into(),
                spec: Some(spec),
                ..Default::default()
            })
            .await?
            .into_inner();
        Ok(response
            .sandbox
            .as_ref()
            .map(SandboxInfo::from_proto)
            .unwrap_or(SandboxInfo {
                id: String::new(),
                name: name.into(),
                state: State::Transitioning,
                phase: "provisioning".into(),
                image: image.into(),
            }))
    }

    pub async fn start(&mut self, name: &str) -> Result<(), Error> {
        self.client
            .start_sandbox(StartSandboxRequest {
                workspace_scope: workspace(),
                name: name.into(),
                ..Default::default()
            })
            .await?;
        Ok(())
    }

    pub async fn stop(&mut self, name: &str) -> Result<(), Error> {
        self.client
            .stop_sandbox(StopSandboxRequest {
                workspace_scope: workspace(),
                name: name.into(),
                ..Default::default()
            })
            .await?;
        Ok(())
    }

    pub async fn delete(&mut self, name: &str) -> Result<(), Error> {
        self.client
            .delete_sandbox(DeleteSandboxRequest {
                workspace_scope: workspace(),
                name: name.into(),
                allow_missing: true,
                ..Default::default()
            })
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openshell_core::proto::datamodel::v1::ObjectMeta;
    use openshell_core::proto::{SandboxStatus, SandboxTemplate};

    #[test]
    fn phases_map_to_app_states() {
        assert_eq!(State::from_phase(SandboxPhase::Ready), State::Running);
        assert_eq!(State::from_phase(SandboxPhase::Stopped), State::Stopped);
        assert_eq!(State::from_phase(SandboxPhase::Error), State::Failed);
        assert_eq!(
            State::from_phase(SandboxPhase::Starting),
            State::Transitioning
        );
    }

    #[test]
    fn sandbox_info_reads_metadata_phase_and_image() {
        let sandbox = Sandbox {
            metadata: Some(ObjectMeta {
                id: "id1".into(),
                name: "dev".into(),
                ..Default::default()
            }),
            spec: Some(SandboxSpec {
                template: Some(SandboxTemplate {
                    image: "ubuntu:24.04".into(),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            status: Some(SandboxStatus {
                phase: SandboxPhase::Ready as i32,
                ..Default::default()
            }),
            ..Default::default()
        };
        let info = SandboxInfo::from_proto(&sandbox);
        assert_eq!(
            (
                info.name.as_str(),
                info.state,
                info.phase.as_str(),
                info.image.as_str()
            ),
            ("dev", State::Running, "ready", "ubuntu:24.04")
        );
    }

    #[test]
    fn missing_tls_material_is_reported_before_connecting() {
        let config = Config {
            endpoint: "https://127.0.0.1:1".into(),
            tls_dir: "/nonexistent/tls".into(),
        };
        let runtime = std::thread::spawn(move || {
            let rt = tokio_free_block_on(Gateway::connect(&config));
            rt.err().map(|e| e.to_string())
        });
        assert!(runtime.join().unwrap().unwrap().contains("not found"));
    }

    /// `connect` returns before awaiting anything when the CA is absent, so a
    /// single poll completes it and no runtime is needed.
    fn tokio_free_block_on<F: std::future::Future>(fut: F) -> F::Output {
        let mut fut = std::pin::pin!(fut);
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        match fut.as_mut().poll(&mut cx) {
            std::task::Poll::Ready(v) => v,
            std::task::Poll::Pending => panic!("future was not immediately ready"),
        }
    }
}
