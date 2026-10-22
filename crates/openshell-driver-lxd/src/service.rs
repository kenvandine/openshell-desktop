//! The `ComputeDriver` gRPC service the gateway calls.

use std::pin::Pin;

use futures::Stream;
use tokio::sync::broadcast::error::RecvError;
use tonic::{Request, Response, Status};
use tracing::warn;

use crate::driver::{DriverError, LxdDriver};
use crate::proto::compute::v1::compute_driver_server::ComputeDriver;
use crate::proto::compute::v1::{
    AuthenticateSandboxRequest, AuthenticateSandboxResponse, CpuResourceCapabilities,
    CreateSandboxRequest, CreateSandboxResponse, DeleteSandboxRequest, DeleteSandboxResponse,
    DeleteWorkspaceRequest, DeleteWorkspaceResponse, EnsureWorkspaceRequest,
    EnsureWorkspaceResponse, GetCapabilitiesRequest, GetCapabilitiesResponse, GetSandboxRequest,
    GetSandboxResponse, GpuResourceCapabilities, ListSandboxesRequest, ListSandboxesResponse,
    MemoryResourceCapabilities, ResourceCapabilities, StartSandboxRequest, StartSandboxResponse,
    StopSandboxRequest, StopSandboxResponse, ValidateSandboxCreateRequest,
    ValidateSandboxCreateResponse, WatchSandboxesEvent, WatchSandboxesRequest,
    WatchSandboxesSandboxEvent, watch_sandboxes_event,
};
use crate::proto::extension::v1::{PeerMetadata, ProtocolVersion};

pub const DRIVER_NAME: &str = "lxd";
pub const IMPLEMENTATION_NAME: &str = "openshell-desktop/lxd";
pub const PROTOCOL_MAJOR: u32 = 1;
pub const PROTOCOL_MINOR: u32 = 0;
pub const CONTRACT_CAPABILITY: &str = "openshell.compute.contract";

impl From<DriverError> for Status {
    fn from(error: DriverError) -> Self {
        match error {
            DriverError::InvalidArgument(m) => Status::invalid_argument(m),
            DriverError::Precondition(m) => Status::failed_precondition(m),
            DriverError::NotFound(m) => Status::not_found(m),
            DriverError::AlreadyExists(m) => Status::already_exists(m),
            DriverError::Unavailable(m) => Status::unavailable(m),
            DriverError::Internal(m) => Status::internal(m),
        }
    }
}

pub struct Service {
    driver: LxdDriver,
}

impl Service {
    pub fn new(driver: LxdDriver) -> Self {
        Self { driver }
    }

    pub fn capabilities(&self) -> GetCapabilitiesResponse {
        let config = self.driver.config();
        GetCapabilitiesResponse {
            driver_name: DRIVER_NAME.into(),
            driver_version: env!("CARGO_PKG_VERSION").into(),
            default_image: config.default_image.clone(),
            gateway_manages_lifecycle: true,
            supports_sandbox_authentication: false,
            driver_reports_runtime_readiness: false,
            resource_capabilities: Some(ResourceCapabilities {
                cpu: Some(CpuResourceCapabilities {
                    limit_supported: true,
                }),
                memory: Some(MemoryResourceCapabilities {
                    limit_supported: true,
                }),
                gpu: Some(GpuResourceCapabilities {
                    default_selection_supported: config.gpu.is_some(),
                    count_selection_supported: false,
                }),
            }),
            rootfs_tar_staging_dir: String::new(),
            rootfs_tar_max_bytes: 0,
            extension: Some(extension_metadata()),
            resource_admission_policy: config.admission.acknowledgement(),
        }
    }
}

pub fn extension_metadata() -> PeerMetadata {
    PeerMetadata {
        protocol_version: Some(ProtocolVersion {
            major: PROTOCOL_MAJOR,
            minor: PROTOCOL_MINOR,
        }),
        implementation_name: IMPLEMENTATION_NAME.into(),
        implementation_version: env!("CARGO_PKG_VERSION").into(),
        supported_capabilities: vec![CONTRACT_CAPABILITY.into()],
        required_capabilities: vec![CONTRACT_CAPABILITY.into()],
    }
}

/// Checks the gateway's side of the extension handshake: same protocol
/// major version, and each peer offers what the other requires.
pub fn check_gateway(gateway: Option<&PeerMetadata>) -> Result<(), Status> {
    let gateway = gateway.ok_or_else(|| {
        Status::failed_precondition(
            "gateway did not send extension metadata; upgrade the gateway and driver together",
        )
    })?;
    let version = gateway
        .protocol_version
        .as_ref()
        .ok_or_else(|| Status::failed_precondition("gateway did not send a protocol version"))?;
    if version.major != PROTOCOL_MAJOR {
        return Err(Status::failed_precondition(format!(
            "gateway speaks compute protocol {}.{}; this driver speaks {PROTOCOL_MAJOR}.{PROTOCOL_MINOR}",
            version.major, version.minor
        )));
    }
    let ours = extension_metadata();
    let missing: Vec<&str> = gateway
        .required_capabilities
        .iter()
        .filter(|c| !ours.supported_capabilities.contains(c))
        .map(String::as_str)
        .collect();
    if !missing.is_empty() {
        return Err(Status::failed_precondition(format!(
            "gateway requires capabilities this driver lacks: {}",
            missing.join(", ")
        )));
    }
    if !gateway
        .supported_capabilities
        .iter()
        .any(|c| c == CONTRACT_CAPABILITY)
    {
        return Err(Status::failed_precondition(format!(
            "gateway does not support {CONTRACT_CAPABILITY}"
        )));
    }
    Ok(())
}

type WatchStream = Pin<Box<dyn Stream<Item = Result<WatchSandboxesEvent, Status>> + Send>>;

#[tonic::async_trait]
impl ComputeDriver for Service {
    async fn get_capabilities(
        &self,
        request: Request<GetCapabilitiesRequest>,
    ) -> Result<Response<GetCapabilitiesResponse>, Status> {
        check_gateway(request.get_ref().gateway.as_ref())?;
        Ok(Response::new(self.capabilities()))
    }

    async fn authenticate_sandbox(
        &self,
        _request: Request<AuthenticateSandboxRequest>,
    ) -> Result<Response<AuthenticateSandboxResponse>, Status> {
        Err(Status::unimplemented(
            "the LXD driver does not authenticate sandbox credentials",
        ))
    }

    async fn validate_sandbox_create(
        &self,
        request: Request<ValidateSandboxCreateRequest>,
    ) -> Result<Response<ValidateSandboxCreateResponse>, Status> {
        let sandbox = request
            .into_inner()
            .sandbox
            .ok_or_else(|| Status::invalid_argument("sandbox is required"))?;
        self.driver.validate(&sandbox)?;
        Ok(Response::new(ValidateSandboxCreateResponse {}))
    }

    async fn get_sandbox(
        &self,
        request: Request<GetSandboxRequest>,
    ) -> Result<Response<GetSandboxResponse>, Status> {
        let sandbox = self
            .driver
            .get(&request.into_inner().sandbox_id)
            .await?
            .ok_or_else(|| Status::not_found("sandbox not found"))?;
        Ok(Response::new(GetSandboxResponse {
            sandbox: Some(sandbox),
        }))
    }

    async fn list_sandboxes(
        &self,
        _request: Request<ListSandboxesRequest>,
    ) -> Result<Response<ListSandboxesResponse>, Status> {
        Ok(Response::new(ListSandboxesResponse {
            sandboxes: self.driver.list().await?,
        }))
    }

    async fn create_sandbox(
        &self,
        request: Request<CreateSandboxRequest>,
    ) -> Result<Response<CreateSandboxResponse>, Status> {
        let sandbox = request
            .into_inner()
            .sandbox
            .ok_or_else(|| Status::invalid_argument("sandbox is required"))?;
        self.driver.create(&sandbox).await?;
        Ok(Response::new(CreateSandboxResponse {
            runtime_identity: String::new(),
        }))
    }

    async fn stop_sandbox(
        &self,
        request: Request<StopSandboxRequest>,
    ) -> Result<Response<StopSandboxResponse>, Status> {
        self.driver.stop(&request.into_inner().sandbox_id).await?;
        Ok(Response::new(StopSandboxResponse {}))
    }

    async fn start_sandbox(
        &self,
        request: Request<StartSandboxRequest>,
    ) -> Result<Response<StartSandboxResponse>, Status> {
        let request = request.into_inner();
        if request.isolation_backend.is_some() {
            return Err(Status::failed_precondition(
                "the LXD driver does not support external isolation backends",
            ));
        }
        self.driver
            .start(
                &request.sandbox_id,
                &request.generation_id,
                &request.launch_authentication,
            )
            .await?;
        Ok(Response::new(StartSandboxResponse {
            runtime_identity: String::new(),
        }))
    }

    async fn delete_sandbox(
        &self,
        request: Request<DeleteSandboxRequest>,
    ) -> Result<Response<DeleteSandboxResponse>, Status> {
        let deleted = self.driver.delete(&request.into_inner().sandbox_id).await?;
        Ok(Response::new(DeleteSandboxResponse { deleted }))
    }

    type WatchSandboxesStream = WatchStream;

    async fn watch_sandboxes(
        &self,
        _request: Request<WatchSandboxesRequest>,
    ) -> Result<Response<Self::WatchSandboxesStream>, Status> {
        // Subscribe before listing so changes in between are not lost.
        let mut events = self.driver.subscribe();
        let snapshot = self.driver.list().await?;
        let driver = self.driver.clone();
        let stream = async_stream(move |tx| async move {
            for sandbox in snapshot {
                let event = WatchSandboxesEvent {
                    payload: Some(watch_sandboxes_event::Payload::Sandbox(
                        WatchSandboxesSandboxEvent {
                            sandbox: Some(sandbox),
                        },
                    )),
                };
                if tx.send(Ok(event)).await.is_err() {
                    return;
                }
            }
            loop {
                match events.recv().await {
                    Ok(event) => {
                        if tx.send(Ok(event)).await.is_err() {
                            return;
                        }
                    }
                    Err(RecvError::Lagged(skipped)) => {
                        // Fell behind; a fresh listing brings the gateway level.
                        warn!(skipped, "watch subscriber lagged; resending snapshot");
                        match driver.list().await {
                            Ok(sandboxes) => {
                                for sandbox in sandboxes {
                                    let event = WatchSandboxesEvent {
                                        payload: Some(watch_sandboxes_event::Payload::Sandbox(
                                            WatchSandboxesSandboxEvent {
                                                sandbox: Some(sandbox),
                                            },
                                        )),
                                    };
                                    if tx.send(Ok(event)).await.is_err() {
                                        return;
                                    }
                                }
                            }
                            Err(error) => {
                                let _ = tx.send(Err(error.into())).await;
                                return;
                            }
                        }
                    }
                    Err(RecvError::Closed) => return,
                }
            }
        });
        Ok(Response::new(stream))
    }

    async fn ensure_workspace(
        &self,
        _request: Request<EnsureWorkspaceRequest>,
    ) -> Result<Response<EnsureWorkspaceResponse>, Status> {
        // Workspaces share the driver's LXD project; the workspace is recorded
        // on each instance. Nothing to provision.
        Ok(Response::new(EnsureWorkspaceResponse {}))
    }

    async fn delete_workspace(
        &self,
        _request: Request<DeleteWorkspaceRequest>,
    ) -> Result<Response<DeleteWorkspaceResponse>, Status> {
        Ok(Response::new(DeleteWorkspaceResponse {}))
    }
}

/// Runs `body` as a task feeding a channel-backed stream.
fn async_stream<F, Fut>(body: F) -> WatchStream
where
    F: FnOnce(tokio::sync::mpsc::Sender<Result<WatchSandboxesEvent, Status>>) -> Fut,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let (tx, rx) = tokio::sync::mpsc::channel(64);
    tokio::spawn(body(tx));
    Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gateway() -> PeerMetadata {
        PeerMetadata {
            protocol_version: Some(ProtocolVersion { major: 1, minor: 0 }),
            implementation_name: "openshell/gateway".into(),
            implementation_version: "0.0.0".into(),
            supported_capabilities: vec![
                CONTRACT_CAPABILITY.into(),
                "openshell.compute.launch-authentication".into(),
                "openshell.compute.isolation-backend".into(),
            ],
            required_capabilities: vec![CONTRACT_CAPABILITY.into()],
        }
    }

    #[test]
    fn accepts_current_gateway() {
        assert!(check_gateway(Some(&gateway())).is_ok());
    }

    #[test]
    fn rejects_incompatible_gateways() {
        assert!(check_gateway(None).is_err());
        let mut newer = gateway();
        newer.protocol_version = Some(ProtocolVersion { major: 2, minor: 0 });
        assert!(check_gateway(Some(&newer)).is_err());
        let mut demanding = gateway();
        demanding
            .required_capabilities
            .push("openshell.compute.something-new".into());
        assert!(check_gateway(Some(&demanding)).is_err());
    }
}
