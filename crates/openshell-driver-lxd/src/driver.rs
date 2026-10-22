//! Sandbox lifecycle on LXD.
//!
//! A sandbox is two unprivileged LXD containers, the same topology OpenShell's
//! Docker and Podman drivers use:
//!
//! - the **workload** (`openshell-<id>`) runs the `openshell-sandbox` runtime
//!   as init under the resolved non-root identity. It is created with no
//!   profiles, so it gets a root disk and no network device at all: the
//!   workload has no route out;
//! - the **supervisor** (`openshell-sup-<id>`) runs `openshell-supervisor`,
//!   which holds the gateway session, policy engine and egress proxy, and
//!   reaches the host network.
//!
//! They meet on a custom storage volume that holds a Unix control socket,
//! secured with per-sandbox mTLS material the driver stages through LXD's file
//! API before either container starts. The workload keeps the agent stopped
//! until the authenticated supervisor confirms the boundary.

use std::collections::{BTreeSet, HashMap};
use std::net::{IpAddr, Ipv4Addr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use openshell_core::jwt::SandboxLaunchAuthentication;
use openshell_core::resource_admission::DriverAdmissionConfig as AdmissionConfig;
use openshell_core::sandbox_env;
use openshell_isolation_interface::contract::ResolvedWorkloadIdentity as Identity;
use serde_json::{Value, json};
use tokio::sync::broadcast;
use tracing::{debug, info, warn};

use crate::isolation::{self, Entry};
use crate::lxd::{self, FileMeta, Instance, LxdError};
use crate::oci::{ExtractedFile, ImageError, ImportedImage, Importer, RuntimeConfig};
use crate::proto::compute::v1::{
    DriverCondition, DriverSandbox, DriverSandboxSpec, DriverSandboxStatus,
    ResolvedWorkloadIdentity as StatusIdentity, WatchSandboxesDeletedEvent, WatchSandboxesEvent,
    WatchSandboxesSandboxEvent, watch_sandboxes_event,
};
use crate::quantity;

pub const DEFAULT_IMAGE: &str = "nvcr.io/nvidia/base/ubuntu:24.04";

/// Instance name prefix; also how the event pump spots our instances.
const NAME_PREFIX: &str = "openshell-";
const SUPERVISOR_PREFIX: &str = "openshell-sup-";
const VOLUME_PREFIX: &str = "openshell-control-";

const KEY_MANAGED_BY: &str = "user.openshell.managed-by";
const KEY_SANDBOX_ID: &str = "user.openshell.sandbox-id";
const KEY_SANDBOX_NAME: &str = "user.openshell.sandbox-name";
const KEY_WORKSPACE: &str = "user.openshell.workspace";
const KEY_LABELS: &str = "user.openshell.labels";
const KEY_IMAGE: &str = "user.openshell.image";
const KEY_IMAGE_DIGEST: &str = "user.openshell.image-digest";
/// `workload` or `supervisor`.
const KEY_ROLE: &str = "user.openshell.role";
/// `running` or `stopped`: what the gateway last asked for. Tells a stop the
/// gateway requested apart from the workload exiting on its own.
const KEY_DESIRED_STATE: &str = "user.openshell.desired-state";
/// `true` while the driver is bringing the pair up, so a half-started pair is
/// not mistaken for a dead supervisor.
const KEY_STARTING: &str = "user.openshell.starting";
const KEY_UID: &str = "user.openshell.uid";
const KEY_GID: &str = "user.openshell.gid";
const KEY_GROUPS: &str = "user.openshell.supplementary-gids";
const KEY_IDENTITY_SOURCE: &str = "user.openshell.identity-source";
const MANAGED_BY: &str = "openshell";

// Ready-condition reasons, matching OpenShell's container drivers so the
// gateway's restart recovery treats them the same way.
const CONDITION_RUNNING: &str = "ContainerRunning";
const CONDITION_STOPPED: &str = "ContainerStopped";
const CONDITION_EXITED: &str = "ContainerExited";
const CONDITION_STARTING: &str = "Starting";

const RUNTIME_IMAGE_BINARY: &str = "/openshell-sandbox";
const SUPERVISOR_ENTRYPOINT: &str = "/openshell-supervisor";
const HEALTH_SOCKET: &str = "/run/openshell/supervisor-health.sock";
const PROXY_TLS_DIR: &str = "/run/openshell/proxy-tls";

#[derive(Debug)]
pub enum DriverError {
    InvalidArgument(String),
    Precondition(String),
    NotFound(String),
    AlreadyExists(String),
    Unavailable(String),
    Internal(String),
}

impl std::fmt::Display for DriverError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DriverError::InvalidArgument(m)
            | DriverError::Precondition(m)
            | DriverError::NotFound(m)
            | DriverError::AlreadyExists(m)
            | DriverError::Unavailable(m)
            | DriverError::Internal(m) => f.write_str(m),
        }
    }
}

impl From<LxdError> for DriverError {
    fn from(error: LxdError) -> Self {
        match error {
            LxdError::Connection(_) => DriverError::Unavailable(error.to_string()),
            LxdError::NotFound(_) => DriverError::NotFound(error.to_string()),
            LxdError::Api { .. } => DriverError::Internal(error.to_string()),
        }
    }
}

impl From<ImageError> for DriverError {
    fn from(error: ImageError) -> Self {
        match error {
            ImageError::InvalidReference(_) => DriverError::InvalidArgument(error.to_string()),
            ImageError::Lxd(error) => error.into(),
            ImageError::Registry(_) | ImageError::Unpack(_) => {
                DriverError::Precondition(error.to_string())
            }
        }
    }
}

type Result<T> = std::result::Result<T, DriverError>;

/// Where sandbox images come from. The registry-backed importer in
/// production; tests substitute fixed images.
#[tonic::async_trait]
pub trait ImageSource: Send + Sync {
    /// Makes sure LXD holds an image for the OCI `reference`.
    async fn ensure(
        &self,
        lxd: &lxd::Client,
        reference: &str,
    ) -> std::result::Result<ImportedImage, ImageError>;

    /// Reads one file out of an OCI image without importing it into LXD.
    async fn extract_file(
        &self,
        reference: &str,
        path: &str,
    ) -> std::result::Result<ExtractedFile, ImageError>;
}

#[tonic::async_trait]
impl ImageSource for Importer {
    async fn ensure(
        &self,
        lxd: &lxd::Client,
        reference: &str,
    ) -> std::result::Result<ImportedImage, ImageError> {
        Importer::ensure(self, lxd, reference).await
    }

    async fn extract_file(
        &self,
        reference: &str,
        path: &str,
    ) -> std::result::Result<ExtractedFile, ImageError> {
        Importer::extract_file(self, reference, path).await
    }
}

/// The LXD managed network the supervisor joins by default.
pub const DEFAULT_SUPERVISOR_NETWORK: &str = "lxdbr0";

/// Name the supervisor dials for the gateway; `/etc/hosts` maps it to the
/// host address, and the gateway's certificate carries it.
pub const HOST_ALIAS: &str = "host.openshell.internal";

/// How the supervisor reaches the host network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SupervisorNetwork {
    /// Share the host's network namespace, as OpenShell's Podman driver does
    /// (`netns host`). LXD only allows this for privileged containers:
    /// liblxc cannot mount sysfs for an unprivileged one.
    Host,
    /// Attach to this LXD managed network with a bridged NIC. The supervisor
    /// image has no DHCP client, so it gets a static address that LXD
    /// reserves on the network and liblxc configures before init runs.
    Bridge(String),
}

impl Default for SupervisorNetwork {
    fn default() -> Self {
        Self::Bridge(DEFAULT_SUPERVISOR_NETWORK.into())
    }
}

#[derive(Debug, Clone)]
pub struct DriverConfig {
    pub storage_pool: String,
    pub default_image: String,
    pub admission: AdmissionConfig,
    /// LXD `gpu` device selector (`id`, or `pci` when it contains `:`) used
    /// for GPU requests. GPU requests are refused when unset.
    pub gpu: Option<String>,
    /// Sets `nvidia.runtime=true` on sandboxes that request a GPU.
    pub nvidia_runtime: bool,
    /// Seconds to wait for a clean stop before forcing it.
    pub stop_timeout_secs: i64,
    /// Gateway gRPC endpoint as the supervisor sees it.
    pub grpc_endpoint: String,
    /// Gateway CA certificate (PEM) the supervisor trusts, when the gateway
    /// serves TLS.
    pub tls_ca: Option<PathBuf>,
    /// OCI image that ships `/openshell-sandbox`, the workload runtime.
    pub sandbox_runtime_image: String,
    /// OCI image that ships `/openshell-supervisor`.
    pub supervisor_image: String,
    pub network: SupervisorNetwork,
    /// Address the supervisor dials for the host. Defaults to loopback for
    /// [`SupervisorNetwork::Host`] and the bridge's address for a bridge.
    pub host_gateway_ip: Option<IpAddr>,
    /// `/etc/resolv.conf` content for the supervisor. Defaults to the
    /// bridge's DNS server for [`SupervisorNetwork::Bridge`].
    pub resolv_conf: Option<Vec<u8>>,
}

impl Default for DriverConfig {
    fn default() -> Self {
        Self {
            storage_pool: "default".into(),
            default_image: DEFAULT_IMAGE.into(),
            admission: AdmissionConfig::default(),
            gpu: None,
            nvidia_runtime: false,
            stop_timeout_secs: 30,
            grpc_endpoint: String::new(),
            tls_ca: None,
            sandbox_runtime_image: openshell_core::config::default_sandbox_runtime_image(),
            supervisor_image: openshell_core::config::default_supervisor_image(),
            network: SupervisorNetwork::default(),
            host_gateway_ip: None,
            resolv_conf: None,
        }
    }
}

#[derive(Clone)]
pub struct LxdDriver {
    lxd: lxd::Client,
    images: Arc<dyn ImageSource>,
    config: Arc<DriverConfig>,
    events: broadcast::Sender<WatchSandboxesEvent>,
    /// Workload instance name → sandbox ID, so a deletion event (which only
    /// carries the instance name) can be reported by sandbox ID.
    known: Arc<Mutex<HashMap<String, String>>>,
}

/// Images and the runtime binary a create needs.
struct Artifacts {
    workload: ImportedImage,
    supervisor: ImportedImage,
    runtime: ExtractedFile,
}

impl LxdDriver {
    pub fn new(lxd: lxd::Client, images: Arc<dyn ImageSource>, config: DriverConfig) -> Self {
        let (events, _) = broadcast::channel(256);
        Self {
            lxd,
            images,
            config: Arc::new(config),
            events,
            known: Arc::default(),
        }
    }

    pub fn config(&self) -> &DriverConfig {
        &self.config
    }

    pub fn lxd(&self) -> &lxd::Client {
        &self.lxd
    }

    /// Checks a create request without touching LXD.
    pub fn validate(&self, sandbox: &DriverSandbox) -> Result<()> {
        self.plan(sandbox).map(drop)
    }

    pub async fn create(&self, sandbox: &DriverSandbox) -> Result<()> {
        let plan = self.plan(sandbox)?;
        if self.config.grpc_endpoint.trim().is_empty() {
            return Err(DriverError::Precondition(
                "no gateway endpoint configured for the supervisor; start the driver with --grpc-endpoint".into(),
            ));
        }
        let launch = decode_launch_authentication(
            sandbox
                .spec
                .as_ref()
                .map_or(&[][..], |spec| &spec.launch_authentication),
        )?;
        let names = Names::new(&sandbox.id);
        for name in [&names.workload, &names.supervisor] {
            if let Some(existing) = self.lxd.instance(name).await? {
                if existing.config.get(KEY_SANDBOX_ID) == Some(&sandbox.id) {
                    return Err(DriverError::AlreadyExists(format!(
                        "sandbox {} already exists",
                        sandbox.id
                    )));
                }
                return Err(DriverError::Precondition(format!(
                    "LXD instance {name} exists and is not managed by this driver"
                )));
            }
        }

        let artifacts = self.fetch_artifacts(&plan).await?;
        info!(
            sandbox_id = %sandbox.id,
            instance = %names.workload,
            image = %plan.image,
            digest = %artifacts.workload.manifest_digest,
            "creating sandbox"
        );

        self.lxd
            .create_volume(&self.config.storage_pool, &names.volume)
            .await?;
        // Everything after the first LXD object exists must clean up on failure.
        let result = self
            .provision(sandbox, &plan, &names, &artifacts, &launch)
            .await;
        if let Err(error) = result {
            // A container that exited during launch says why on its console,
            // which goes with it on cleanup.
            let error = self.with_consoles(error, &names).await;
            warn!(sandbox_id = %sandbox.id, %error, "sandbox create failed; removing instances");
            self.remove_all(&names).await;
            return Err(error);
        }
        Ok(())
    }

    async fn fetch_artifacts(&self, plan: &Plan) -> Result<Artifacts> {
        let runtime = self
            .images
            .extract_file(&self.config.sandbox_runtime_image, RUNTIME_IMAGE_BINARY)
            .await?;
        if !runtime.content.starts_with(b"\x7fELF") {
            return Err(DriverError::Precondition(format!(
                "{RUNTIME_IMAGE_BINARY} in {} is not a Linux executable",
                self.config.sandbox_runtime_image
            )));
        }
        let supervisor = self
            .images
            .ensure(&self.lxd, &self.config.supervisor_image)
            .await?;
        let workload = self.images.ensure(&self.lxd, &plan.image).await?;
        Ok(Artifacts {
            workload,
            supervisor,
            runtime,
        })
    }

    async fn host_gateway_ip(&self) -> Result<IpAddr> {
        if let Some(ip) = self.config.host_gateway_ip {
            return Ok(ip);
        }
        match &self.config.network {
            SupervisorNetwork::Host => Ok(IpAddr::from([127, 0, 0, 1])),
            SupervisorNetwork::Bridge(network) => self
                .lxd
                .network_ipv4(network)
                .await?
                .and_then(|ip| ip.parse().ok())
                .ok_or_else(|| {
                    DriverError::Precondition(format!(
                        "LXD network {network} has no IPv4 address; pass --host-gateway-ip"
                    ))
                }),
        }
    }

    /// A free address on the supervisor's bridge, or `None` on the host
    /// network.
    async fn supervisor_address(&self, sandbox_id: &str) -> Result<Option<StaticAddress>> {
        let SupervisorNetwork::Bridge(network) = &self.config.network else {
            return Ok(None);
        };
        let (gateway, prefix) = self.lxd.network_ipv4_cidr(network).await?.ok_or_else(|| {
            DriverError::Precondition(format!("LXD network {network} has no IPv4 subnet"))
        })?;
        let used = self.lxd.network_leases(network).await?;
        let address = pick_address(gateway, prefix, &used, sandbox_id).ok_or_else(|| {
            DriverError::Precondition(format!("LXD network {network} has no free IPv4 address"))
        })?;
        Ok(Some(StaticAddress {
            address,
            prefix,
            gateway,
        }))
    }

    /// Creates, stages and starts both instances.
    async fn provision(
        &self,
        sandbox: &DriverSandbox,
        plan: &Plan,
        names: &Names,
        artifacts: &Artifacts,
        launch: &SandboxLaunchAuthentication,
    ) -> Result<()> {
        let host_gateway_ip = self.host_gateway_ip().await?;
        let address = self.supervisor_address(&sandbox.id).await?;
        self.lxd
            .create_instance(&self.workload_body(names, sandbox, plan, &artifacts.workload))
            .await?;
        self.known
            .lock()
            .unwrap()
            .insert(names.workload.clone(), sandbox.id.clone());

        // Never trust image content in the reserved hierarchy: an image
        // symlink there could alias the channel into the agent's tree.
        if self
            .lxd
            .read_file(&names.workload, "/.openshell")
            .await?
            .is_some()
        {
            return Err(DriverError::Precondition(
                "workload images must not prepopulate the reserved /.openshell hierarchy".into(),
            ));
        }
        let passwd = self
            .lxd
            .read_file(&names.workload, "/etc/passwd")
            .await?
            .unwrap_or_default();
        let group = self
            .lxd
            .read_file(&names.workload, "/etc/group")
            .await?
            .unwrap_or_default();
        let identity = resolve_identity(
            sandbox.spec.as_ref(),
            &artifacts.workload.manifest_digest,
            artifacts.workload.config.user.as_deref().unwrap_or(""),
            &String::from_utf8_lossy(&passwd),
            &String::from_utf8_lossy(&group),
        )?;
        let workspace = openshell_core::driver_mounts::resolve_oci_workspace_root(
            artifacts
                .workload
                .config
                .working_dir
                .as_deref()
                .unwrap_or(""),
        )
        .map_err(DriverError::Precondition)?;

        self.lxd
            .create_instance(&self.supervisor_body(
                names,
                sandbox,
                &artifacts.supervisor,
                &identity,
                address.as_ref(),
            ))
            .await?;

        // Runtime binary and workspace on the workload's root filesystem.
        for dir in ["/opt", "/opt/openshell", "/opt/openshell/bin"] {
            self.lxd.make_dir(&names.workload, dir, ROOT_DIR).await?;
        }
        self.lxd
            .push_file(
                &names.workload,
                openshell_core::driver_utils::SUPERVISOR_CONTAINER_BINARY,
                &artifacts.runtime.content,
                FileMeta {
                    uid: 0,
                    gid: 0,
                    mode: 0o555,
                },
            )
            .await?;
        if workspace == openshell_core::driver_mounts::DEFAULT_WORKSPACE_ROOT {
            self.lxd
                .ensure_dir(
                    &names.workload,
                    &workspace,
                    FileMeta {
                        uid: identity.uid,
                        gid: identity.gid,
                        mode: 0o700,
                    },
                )
                .await?;
        }

        let child_env = child_environment(sandbox, artifacts.workload.config.env.as_deref());
        self.stage(
            names,
            sandbox,
            &identity,
            &Stage {
                generation: &uuid::Uuid::new_v4().to_string(),
                child_env,
                launch,
                host_gateway_ip,
            },
        )
        .await?;
        self.stage_supervisor_extras(names, sandbox, &identity, &workspace, host_gateway_ip)
            .await?;

        // Instance configuration that depends on the resolved identity.
        let mut workload_config = HashMap::from([
            (
                "raw.lxc".to_owned(),
                raw_lxc(
                    &[
                        openshell_core::driver_utils::SUPERVISOR_CONTAINER_BINARY.to_owned(),
                        "--bootstrap".into(),
                        isolation::BOOTSTRAP_PATH.into(),
                    ],
                    &identity,
                    "/",
                    &WORKLOAD_NETWORK.map(str::to_owned),
                )?,
            ),
            (KEY_UID.to_owned(), identity.uid.to_string()),
            (KEY_GID.to_owned(), identity.gid.to_string()),
            (
                KEY_GROUPS.to_owned(),
                join_ids(&identity.supplementary_gids),
            ),
            (KEY_IDENTITY_SOURCE.to_owned(), identity.source.clone()),
            (KEY_STARTING.to_owned(), "true".to_owned()),
        ]);
        // The runtime sets its own environment for the agent; keep the
        // image's out of the privileged runtime.
        workload_config.insert("environment.PATH".into(), DEFAULT_PATH.into());
        self.lxd
            .patch_instance_config(&names.workload, &workload_config)
            .await?;

        let supervisor_config = HashMap::from([
            (
                "raw.lxc".to_owned(),
                raw_lxc(
                    &self.supervisor_command(&workspace),
                    &identity,
                    "/",
                    &supervisor_network_lxc(address.as_ref()),
                )?,
            ),
            (KEY_STARTING.to_owned(), "true".to_owned()),
        ]);
        self.lxd
            .patch_instance_config(&names.supervisor, &supervisor_config)
            .await?;
        let mut env_config = HashMap::new();
        for (key, value) in self.supervisor_environment(sandbox, plan, &artifacts.workload.config) {
            env_config.insert(format!("environment.{key}"), value);
        }
        self.lxd
            .patch_instance_config(&names.supervisor, &env_config)
            .await?;

        self.launch_pair(names, &identity).await
    }

    /// Pushes the bootstrap material into both stopped instances.
    async fn stage(
        &self,
        names: &Names,
        sandbox: &DriverSandbox,
        identity: &Identity,
        stage: &Stage<'_>,
    ) -> Result<()> {
        // Binds the evidence to the workload and checks the fence the
        // instance was really created with.
        let instance = self
            .lxd
            .instance(&names.workload)
            .await?
            .ok_or_else(|| DriverError::NotFound("workload instance disappeared".into()))?;
        let nics = network_devices(&instance);
        let bootstrap = isolation::bootstrap(isolation::BootstrapInput {
            sandbox_id: &sandbox.id,
            instance: &names.workload,
            generation: stage.generation,
            host_gateway_ip: stage.host_gateway_ip,
            identity,
            child_env: stage.child_env.clone(),
            launch_authentication: stage.launch,
        })?;
        isolation::outer_fence_evidence(
            &names.workload,
            &nics,
            &stage.launch.supervisor.runtime_generation.to_string(),
        )?;
        self.push_entries(&names.workload, &bootstrap.workload)
            .await?;
        self.push_entries(&names.supervisor, &bootstrap.supervisor)
            .await
    }

    /// Everything else the supervisor needs on its filesystem.
    async fn stage_supervisor_extras(
        &self,
        names: &Names,
        sandbox: &DriverSandbox,
        identity: &Identity,
        workspace: &str,
        host_gateway_ip: IpAddr,
    ) -> Result<()> {
        let owned = |mode| FileMeta {
            uid: identity.uid,
            gid: identity.gid,
            mode,
        };
        // Writable scratch for a non-root supervisor.
        for dir in ["/run", "/run/openshell"] {
            self.lxd.make_dir(&names.supervisor, dir, ROOT_DIR).await?;
        }
        self.lxd
            .ensure_dir(&names.supervisor, "/run/openshell", owned(0o755))
            .await?;
        let system_file = FileMeta {
            uid: 0,
            gid: 0,
            mode: 0o644,
        };
        let hosts = format!("127.0.0.1 localhost\n::1 localhost\n{host_gateway_ip} {HOST_ALIAS}\n");
        self.lxd
            .push_file(
                &names.supervisor,
                "/etc/hosts",
                hosts.as_bytes(),
                system_file,
            )
            .await?;
        // The bridge's dnsmasq answers DNS; the host's stub resolver is
        // loopback only.
        let resolv = match (&self.config.resolv_conf, &self.config.network) {
            (Some(resolv), _) => Some(resolv.clone()),
            (None, SupervisorNetwork::Bridge(_)) => {
                Some(format!("nameserver {host_gateway_ip}\n").into_bytes())
            }
            (None, SupervisorNetwork::Host) => None,
        };
        if let Some(resolv) = resolv {
            self.lxd
                .push_file(&names.supervisor, "/etc/resolv.conf", &resolv, system_file)
                .await?;
        }
        if let Some(ca) = &self.config.tls_ca {
            let pem = std::fs::read(ca).map_err(|e| {
                DriverError::Precondition(format!("cannot read TLS CA {}: {e}", ca.display()))
            })?;
            self.push_with_parents(
                &names.supervisor,
                openshell_core::driver_utils::TLS_CA_MOUNT_PATH,
                &pem,
                FileMeta {
                    uid: 0,
                    gid: 0,
                    mode: 0o444,
                },
            )
            .await?;
        }
        if let Some(token) = sandbox
            .spec
            .as_ref()
            .map(|spec| spec.sandbox_token.as_str())
            .filter(|token| !token.is_empty())
        {
            self.push_with_parents(
                &names.supervisor,
                openshell_core::driver_utils::SANDBOX_TOKEN_MOUNT_PATH,
                token.as_bytes(),
                owned(0o400),
            )
            .await?;
        }
        debug!(workspace, "staged supervisor files");
        Ok(())
    }

    async fn push_entries(&self, instance: &str, entries: &[Entry]) -> Result<()> {
        for entry in entries {
            match &entry.content {
                None => {
                    self.lxd
                        .ensure_dir(instance, &entry.path, entry.meta)
                        .await?
                }
                Some(content) => {
                    self.lxd
                        .push_file(instance, &entry.path, content, entry.meta)
                        .await?;
                }
            }
        }
        Ok(())
    }

    async fn push_with_parents(
        &self,
        instance: &str,
        path: &str,
        content: &[u8],
        meta: FileMeta,
    ) -> Result<()> {
        let mut prefix = String::new();
        let parent = path.rsplit_once('/').map_or("", |(parent, _)| parent);
        for part in parent.split('/').filter(|part| !part.is_empty()) {
            prefix.push('/');
            prefix.push_str(part);
            self.lxd.make_dir(instance, &prefix, ROOT_DIR).await?;
        }
        Ok(self.lxd.push_file(instance, path, content, meta).await?)
    }

    /// Starts the workload, hands the control directory to its identity, then
    /// starts the supervisor, and marks the pair as running.
    async fn launch_pair(&self, names: &Names, identity: &Identity) -> Result<()> {
        self.lxd
            .set_state(&names.workload, "start", false, -1)
            .await?;
        if self
            .lxd
            .instance(&names.workload)
            .await?
            .is_none_or(|w| w.status != "Running")
        {
            return Err(DriverError::Internal(
                "the sandbox runtime exited while starting".into(),
            ));
        }
        // The control volume's root belongs to root; both containers run as
        // the workload identity, so give it a directory inside the volume.
        // The runtime retries dialing until the supervisor binds the socket.
        self.lxd
            .ensure_dir(
                &names.workload,
                isolation::CONTROL_ROOT,
                isolation::control_dir_meta(identity),
            )
            .await?;
        // A push into a stopped instance lands under the volume's mountpoint,
        // out of the supervisor's sight; fail with the real cause instead.
        let workload = self.lxd.instance(&names.workload).await?;
        if workload.is_none_or(|w| w.status != "Running") {
            return Err(DriverError::Internal(
                "the sandbox runtime exited while starting".into(),
            ));
        }
        if let Err(error) = self
            .lxd
            .set_state(&names.supervisor, "start", false, -1)
            .await
        {
            let _ = self.lxd.set_state(&names.workload, "stop", true, -1).await;
            return Err(error.into());
        }
        let config = HashMap::from([
            (KEY_DESIRED_STATE.to_owned(), "running".to_owned()),
            (KEY_STARTING.to_owned(), "false".to_owned()),
        ]);
        self.lxd
            .patch_instance_config(&names.workload, &config)
            .await?;
        self.lxd
            .patch_instance_config(&names.supervisor, &config)
            .await?;
        Ok(())
    }

    /// Appends the instances' recent console output to `error`.
    async fn with_consoles(&self, error: DriverError, names: &Names) -> DriverError {
        let mut extra = String::new();
        for name in [&names.workload, &names.supervisor] {
            // LXD answers nothing useful while an exiting instance is still
            // being stopped; give it a moment to settle.
            let mut console = String::new();
            for _ in 0..10 {
                let settled = matches!(
                    self.lxd.instance(name).await,
                    Ok(Some(ref i)) if i.status == "Stopped" || i.status == "Running"
                );
                console = self.lxd.console_tail(name, 15).await;
                if settled && !console.is_empty() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            if !console.is_empty() {
                extra.push_str(&format!("\n{name} console:\n{console}"));
            }
        }
        if extra.is_empty() {
            return error;
        }
        let append = |m: String| m + &extra;
        match error {
            DriverError::InvalidArgument(m) => DriverError::InvalidArgument(append(m)),
            DriverError::Precondition(m) => DriverError::Precondition(append(m)),
            DriverError::NotFound(m) => DriverError::NotFound(append(m)),
            DriverError::AlreadyExists(m) => DriverError::AlreadyExists(append(m)),
            DriverError::Unavailable(m) => DriverError::Unavailable(append(m)),
            DriverError::Internal(m) => DriverError::Internal(append(m)),
        }
    }

    /// Removes whatever exists of a sandbox; used on failed creates.
    async fn remove_all(&self, names: &Names) {
        for name in [&names.supervisor, &names.workload] {
            if let Err(error) = self.force_delete(name).await {
                warn!(instance = %name, %error, "could not remove instance");
            }
        }
        if let Err(error) = self
            .lxd
            .delete_volume(&self.config.storage_pool, &names.volume)
            .await
        {
            warn!(volume = %names.volume, %error, "could not remove control volume");
        }
        self.known.lock().unwrap().remove(&names.workload);
    }

    async fn force_delete(&self, name: &str) -> std::result::Result<(), LxdError> {
        match self.lxd.instance(name).await? {
            None => return Ok(()),
            Some(instance) if instance.status != "Stopped" => {
                self.lxd.set_state(name, "stop", true, -1).await?;
            }
            Some(_) => {}
        }
        match self.lxd.delete_instance(name).await {
            Ok(()) | Err(LxdError::NotFound(_)) => Ok(()),
            Err(error) => Err(error),
        }
    }

    /// Starts a stopped sandbox with fresh launch credentials, which are
    /// always required. A running pair restarts only for the generation it
    /// already runs (gateway startup recovery); another generation is refused.
    pub async fn start(
        &self,
        sandbox_id: &str,
        generation_id: &str,
        encoded_authentication: &[u8],
    ) -> Result<()> {
        let generation = openshell_core::sandbox_generation::SandboxGenerationId::parse(
            generation_id.to_owned(),
        )
        .map_err(|e| DriverError::InvalidArgument(e.to_string()))?;
        let launch = decode_launch_authentication(encoded_authentication)?;
        let workload = self.managed(sandbox_id).await?;
        let names = Names::new(sandbox_id);
        let supervisor = self.lxd.instance(&names.supervisor).await?.ok_or_else(|| {
            DriverError::Precondition(format!("sandbox {sandbox_id} has no supervisor"))
        })?;

        let restart = self.restart_metadata(&names).await?;
        if workload.status == "Running"
            && supervisor.status == "Running"
            && restart.generation != generation.as_str()
        {
            return Err(DriverError::Precondition(format!(
                "sandbox is already running generation {}",
                restart.generation
            )));
        }
        // A running pair with the same generation is gateway startup
        // recovery: restart both so the launch session changes together.

        self.set_marker(&names, KEY_DESIRED_STATE, "stopped")
            .await?;
        self.set_marker(&names, KEY_STARTING, "true").await?;
        let result = async {
            self.stop_instance(&supervisor).await?;
            self.stop_instance(&workload).await?;
            let host_gateway_ip = self.host_gateway_ip().await?;
            // The restart copy lives on the supervisor, so the stored
            // identity and child environment need no new image inspection.
            let sandbox = DriverSandbox {
                id: sandbox_id.to_owned(),
                ..Default::default()
            };
            self.stage(
                &names,
                &sandbox,
                &restart.workload_identity,
                &Stage {
                    generation: generation.as_str(),
                    child_env: restart.child_env.clone(),
                    launch: &launch,
                    host_gateway_ip,
                },
            )
            .await?;
            self.launch_pair(&names, &restart.workload_identity).await
        }
        .await;
        if result.is_err() {
            let _ = self.set_marker(&names, KEY_STARTING, "false").await;
        }
        result
    }

    pub async fn stop(&self, sandbox_id: &str) -> Result<()> {
        let workload = self.managed(sandbox_id).await?;
        let names = Names::new(sandbox_id);
        self.set_marker(&names, KEY_DESIRED_STATE, "stopped")
            .await?;
        if let Some(supervisor) = self.lxd.instance(&names.supervisor).await? {
            self.stop_instance(&supervisor).await?;
        }
        self.stop_instance(&workload).await
    }

    /// Returns whether a sandbox was deleted.
    pub async fn delete(&self, sandbox_id: &str) -> Result<bool> {
        if sandbox_id.is_empty() {
            return Err(DriverError::InvalidArgument(
                "sandbox_id is required".into(),
            ));
        }
        let names = Names::new(sandbox_id);
        let existed = match self.managed(sandbox_id).await {
            Ok(_) => true,
            Err(DriverError::NotFound(_)) => false,
            Err(error) => return Err(error),
        };
        // Quiet the fail-closed monitor before taking the pair down.
        if existed {
            self.set_marker(&names, KEY_DESIRED_STATE, "stopped")
                .await?;
        }
        for name in [&names.supervisor, &names.workload] {
            self.force_delete(name).await?;
        }
        self.lxd
            .delete_volume(&self.config.storage_pool, &names.volume)
            .await?;
        self.known.lock().unwrap().remove(&names.workload);
        Ok(existed)
    }

    pub async fn get(&self, sandbox_id: &str) -> Result<Option<DriverSandbox>> {
        match self.managed(sandbox_id).await {
            Ok(workload) => {
                let supervisor = self
                    .lxd
                    .instance(&Names::new(sandbox_id).supervisor)
                    .await?;
                Ok(self.observe(&workload, supervisor.as_ref()))
            }
            Err(DriverError::NotFound(_)) => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub async fn list(&self) -> Result<Vec<DriverSandbox>> {
        let instances = self.lxd.instances().await?;
        Ok(self.observe_all(&instances))
    }

    fn observe_all(&self, instances: &[Instance]) -> Vec<DriverSandbox> {
        let supervisors: HashMap<&str, &Instance> = instances
            .iter()
            .filter(|i| role(i) == Some(isolation::ROLE_SUPERVISOR))
            .filter_map(|i| Some((i.config.get(KEY_SANDBOX_ID)?.as_str(), i)))
            .collect();
        instances
            .iter()
            .filter(|i| role(i) == Some(isolation::ROLE_WORKLOAD))
            .filter_map(|workload| {
                let id = workload.config.get(KEY_SANDBOX_ID)?;
                self.observe(workload, supervisors.get(id.as_str()).copied())
            })
            .collect()
    }

    /// Live sandbox events. Callers subscribe before listing so nothing that
    /// happens in between is lost.
    pub fn subscribe(&self) -> broadcast::Receiver<WatchSandboxesEvent> {
        self.events.subscribe()
    }

    /// Follows LXD lifecycle events forever, reconnecting when the event
    /// websocket drops, and republishes them as sandbox observations.
    pub async fn pump_events(self) {
        loop {
            match self.lxd.lifecycle_events().await {
                Ok(mut events) => {
                    info!("following LXD lifecycle events");
                    self.resync().await;
                    use futures::StreamExt;
                    while let Some(event) = events.next().await {
                        match event {
                            Ok(event) => {
                                if let Some(name) = event.instance
                                    && name.starts_with(NAME_PREFIX)
                                {
                                    debug!(instance = %name, action = %event.action, "LXD event");
                                    self.refresh(&name).await;
                                }
                            }
                            Err(error) => {
                                warn!(%error, "LXD event stream failed");
                                break;
                            }
                        }
                    }
                }
                Err(error) => warn!(%error, "could not subscribe to LXD events"),
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }

    /// Re-reads every sandbox after (re)connecting to the event stream, and
    /// reports any that disappeared while we were not listening.
    async fn resync(&self) {
        let instances = match self.lxd.instances().await {
            Ok(instances) => instances,
            Err(error) => {
                warn!(%error, "could not list LXD instances");
                return;
            }
        };
        let present: BTreeSet<&str> = instances.iter().map(|i| i.name.as_str()).collect();
        let vanished: Vec<String> = self
            .known
            .lock()
            .unwrap()
            .keys()
            .filter(|name| !present.contains(name.as_str()))
            .cloned()
            .collect();
        for name in vanished {
            self.publish_deleted(&name);
        }
        for sandbox in self.observe_all(&instances) {
            self.publish(sandbox);
        }
        let supervisors: HashMap<&str, &Instance> = instances
            .iter()
            .filter(|i| role(i) == Some(isolation::ROLE_SUPERVISOR))
            .filter_map(|i| Some((i.config.get(KEY_SANDBOX_ID)?.as_str(), i)))
            .collect();
        for workload in instances
            .iter()
            .filter(|i| role(i) == Some(isolation::ROLE_WORKLOAD))
        {
            let id = workload.config.get(KEY_SANDBOX_ID).map(String::as_str);
            let supervisor = id.and_then(|id| supervisors.get(id).copied());
            self.enforce(workload, supervisor);
        }
    }

    /// Handles an event for one instance: finds the sandbox it belongs to and
    /// republishes that sandbox.
    async fn refresh(&self, name: &str) {
        let instance = match self.lxd.instance(name).await {
            Ok(Some(instance)) => instance,
            Ok(None) => {
                // A deleted workload ends the sandbox. A deleted supervisor
                // only changes its condition, which the workload's own
                // events and the next resync report.
                self.publish_deleted(name);
                return;
            }
            Err(error) => {
                warn!(instance = %name, %error, "could not read instance after event");
                return;
            }
        };
        let Some(id) = instance.config.get(KEY_SANDBOX_ID).cloned() else {
            return;
        };
        let names = Names::new(&id);
        let (workload, supervisor) = match (
            self.lxd.instance(&names.workload).await,
            self.lxd.instance(&names.supervisor).await,
        ) {
            (Ok(Some(workload)), Ok(supervisor)) => (workload, supervisor),
            (Ok(None), _) => return,
            (Err(error), _) | (_, Err(error)) => {
                warn!(sandbox_id = %id, %error, "could not read sandbox after event");
                return;
            }
        };
        if let Some(sandbox) = self.observe(&workload, supervisor.as_ref()) {
            self.publish(sandbox);
        }
        self.enforce(&workload, supervisor.as_ref());
    }

    /// Fail closed: when the supervisor is gone but the gateway wants the
    /// sandbox running, stop the workload, since nothing enforces policy.
    fn enforce(&self, workload: &Instance, supervisor: Option<&Instance>) {
        if !supervisor_lost(workload, supervisor) {
            return;
        }
        warn!(instance = %workload.name, "supervisor is not running; stopping workload");
        let driver = self.clone();
        let name = workload.name.clone();
        tokio::spawn(async move {
            if let Err(error) = driver.lxd.set_state(&name, "stop", true, -1).await {
                warn!(instance = %name, %error, "could not stop workload without supervisor");
            }
        });
    }

    fn publish(&self, sandbox: DriverSandbox) {
        let _ = self.events.send(WatchSandboxesEvent {
            payload: Some(watch_sandboxes_event::Payload::Sandbox(
                WatchSandboxesSandboxEvent {
                    sandbox: Some(sandbox),
                },
            )),
        });
    }

    fn publish_deleted(&self, name: &str) {
        let Some(sandbox_id) = self.known.lock().unwrap().remove(name) else {
            return;
        };
        let _ = self.events.send(WatchSandboxesEvent {
            payload: Some(watch_sandboxes_event::Payload::Deleted(
                WatchSandboxesDeletedEvent { sandbox_id },
            )),
        });
    }

    /// Turns a managed workload into the gateway's view of a sandbox, and
    /// remembers it for deletion events. `None` for other instances.
    fn observe(&self, workload: &Instance, supervisor: Option<&Instance>) -> Option<DriverSandbox> {
        let sandbox = driver_sandbox(workload, supervisor)?;
        self.known
            .lock()
            .unwrap()
            .insert(workload.name.clone(), sandbox.id.clone());
        Some(sandbox)
    }

    /// The sandbox's workload instance.
    async fn managed(&self, sandbox_id: &str) -> Result<Instance> {
        if sandbox_id.is_empty() {
            return Err(DriverError::InvalidArgument(
                "sandbox_id is required".into(),
            ));
        }
        let name = instance_name(sandbox_id);
        match self.lxd.instance(&name).await? {
            Some(instance)
                if instance.config.get(KEY_SANDBOX_ID).map(String::as_str) == Some(sandbox_id)
                    && role(&instance) == Some(isolation::ROLE_WORKLOAD) =>
            {
                Ok(instance)
            }
            _ => Err(DriverError::NotFound(format!(
                "sandbox {sandbox_id} not found"
            ))),
        }
    }

    async fn restart_metadata(&self, names: &Names) -> Result<isolation::RestartMetadata> {
        let bytes = self
            .lxd
            .read_file(&names.supervisor, isolation::RESTART_METADATA_PATH)
            .await?
            .ok_or_else(|| {
                DriverError::Precondition("sandbox has no restart metadata; recreate it".into())
            })?;
        isolation::restart_metadata_from_slice(&bytes)
    }

    /// Sets one config key on both instances.
    async fn set_marker(&self, names: &Names, key: &str, value: &str) -> Result<()> {
        let config = HashMap::from([(key.to_owned(), value.to_owned())]);
        for name in [&names.workload, &names.supervisor] {
            match self.lxd.patch_instance_config(name, &config).await {
                Ok(()) => {}
                Err(LxdError::NotFound(_)) if name == &names.supervisor => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }

    /// Stops cleanly, then forcibly if the workload ignores the signal.
    async fn stop_instance(&self, instance: &Instance) -> Result<()> {
        if instance.status == "Stopped" {
            return Ok(());
        }
        let timeout = self.config.stop_timeout_secs;
        if let Err(error) = self
            .lxd
            .set_state(&instance.name, "stop", false, timeout)
            .await
        {
            debug!(instance = %instance.name, %error, "clean stop failed; forcing");
            self.lxd.set_state(&instance.name, "stop", true, -1).await?;
        }
        Ok(())
    }

    fn plan(&self, sandbox: &DriverSandbox) -> Result<Plan> {
        if sandbox.id.is_empty() {
            return Err(DriverError::InvalidArgument(
                "sandbox id is required".into(),
            ));
        }
        let spec = sandbox.spec.clone().unwrap_or_default();
        if spec.isolation_backend.is_some() {
            return Err(DriverError::Precondition(
                "the LXD driver does not support external isolation backends".into(),
            ));
        }
        let template = spec.template.clone().unwrap_or_default();
        if template
            .driver_config
            .as_ref()
            .is_some_and(|config| !config.fields.is_empty())
        {
            return Err(DriverError::Precondition(
                if self.config.admission.allow_driver_config {
                    "the LXD driver does not accept driver config yet".into()
                } else {
                    "caller driver config is disabled; a gateway administrator must enable allow_driver_config".into()
                },
            ));
        }

        let image = if template.image.trim().is_empty() {
            self.config.default_image.clone()
        } else {
            template.image.trim().to_owned()
        };

        let mut limits = Vec::new();
        if let Some(resources) = &template.resources {
            if !resources.cpu_limit.is_empty() {
                let millis = quantity::cpu_millis(&resources.cpu_limit)
                    .map_err(DriverError::InvalidArgument)?;
                limits.push((
                    "limits.cpu.allowance".to_owned(),
                    quantity::lxd_cpu_allowance(millis),
                ));
            }
            if !resources.memory_limit.is_empty() {
                let bytes = quantity::memory_bytes(&resources.memory_limit)
                    .map_err(DriverError::InvalidArgument)?;
                limits.push(("limits.memory".to_owned(), bytes.to_string()));
            }
        }

        let gpu = match spec
            .resource_requirements
            .as_ref()
            .and_then(|r| r.gpu.as_ref())
        {
            None => None,
            Some(request) if request.count.unwrap_or(1) != 1 => {
                return Err(DriverError::Precondition(
                    "the LXD driver can attach one GPU per sandbox".into(),
                ));
            }
            Some(_) => Some(self.config.gpu.clone().ok_or_else(|| {
                DriverError::Precondition("no GPU is configured for the LXD driver".into())
            })?),
        };

        for key in template.environment.keys().chain(spec.environment.keys()) {
            if !valid_env_key(key) {
                return Err(DriverError::InvalidArgument(format!(
                    "invalid environment variable name {key:?}"
                )));
            }
        }

        Ok(Plan {
            image,
            limits,
            gpu,
            labels: template.labels.clone(),
        })
    }

    /// The common part of both instance bodies.
    fn base_config(
        &self,
        names: &Names,
        sandbox: &DriverSandbox,
        role: &str,
        image: &ImportedImage,
    ) -> serde_json::Map<String, Value> {
        let mut config = serde_json::Map::new();
        let mut set = |key: &str, value: String| {
            config.insert(key.to_owned(), Value::String(value));
        };
        set(KEY_MANAGED_BY, MANAGED_BY.into());
        set(KEY_ROLE, role.into());
        set(KEY_SANDBOX_ID, sandbox.id.clone());
        set(KEY_SANDBOX_NAME, sandbox.name.clone());
        set(KEY_WORKSPACE, sandbox.workspace.clone());
        set(KEY_IMAGE_DIGEST, image.manifest_digest.clone());
        set(KEY_DESIRED_STATE, "stopped".into());
        set(KEY_STARTING, "true".into());
        set("security.privileged", "false".into());
        set("security.nesting", "false".into());
        let _ = names;
        config
    }

    fn workload_body(
        &self,
        names: &Names,
        sandbox: &DriverSandbox,
        plan: &Plan,
        image: &ImportedImage,
    ) -> Value {
        let mut config = self.base_config(names, sandbox, isolation::ROLE_WORKLOAD, image);
        let mut set = |key: &str, value: String| {
            config.insert(key.to_owned(), Value::String(value));
        };
        set(KEY_IMAGE, plan.image.clone());
        if !plan.labels.is_empty() {
            set(
                KEY_LABELS,
                serde_json::to_string(&plan.labels).unwrap_or_default(),
            );
        }
        for (key, value) in &plan.limits {
            set(key, value.clone());
        }

        let mut devices = json!({
            "root": { "type": "disk", "path": "/", "pool": self.config.storage_pool },
            "control": {
                "type": "disk",
                "pool": self.config.storage_pool,
                "source": names.volume,
                "path": isolation::CONTROL_VOLUME_ROOT,
            },
        });
        if let Some(selector) = &plan.gpu {
            let key = if selector.contains(':') { "pci" } else { "id" };
            let selector = selector.strip_prefix("pci:").unwrap_or(selector);
            devices["gpu"] = json!({ "type": "gpu", "gputype": "physical", key: selector });
            if self.config.nvidia_runtime {
                set("nvidia.runtime", "true".into());
            }
        }

        json!({
            "name": names.workload,
            "type": "container",
            // No profiles: no inherited NIC, so the workload has no network.
            "profiles": [],
            "source": { "type": "image", "alias": image.alias },
            "config": config,
            "devices": devices,
            "start": false,
        })
    }

    fn supervisor_body(
        &self,
        names: &Names,
        sandbox: &DriverSandbox,
        image: &ImportedImage,
        _identity: &Identity,
        address: Option<&StaticAddress>,
    ) -> Value {
        let config = self.base_config(names, sandbox, isolation::ROLE_SUPERVISOR, image);
        let mut devices = json!({
            "root": { "type": "disk", "path": "/", "pool": self.config.storage_pool },
            "control": {
                "type": "disk",
                "pool": self.config.storage_pool,
                "source": names.volume,
                "path": isolation::CONTROL_VOLUME_ROOT,
            },
        });
        if let SupervisorNetwork::Bridge(network) = &self.config.network {
            let mut nic = json!({ "type": "nic", "network": network, "name": "eth0" });
            // Reserves the address so the network's DHCP never leases it.
            if let Some(address) = address {
                nic["ipv4.address"] = json!(address.address.to_string());
            }
            devices["eth0"] = nic;
        }
        json!({
            "name": names.supervisor,
            "type": "container",
            "profiles": [],
            "source": { "type": "image", "alias": image.alias },
            "config": config,
            "devices": devices,
            "start": false,
        })
    }

    fn supervisor_command(&self, workspace: &str) -> Vec<String> {
        vec![
            SUPERVISOR_ENTRYPOINT.to_owned(),
            "--workdir".into(),
            workspace.to_owned(),
            format!(
                "--backend-descriptor-file={}",
                isolation::RUNTIME_DESCRIPTOR_PATH
            ),
            format!("--auth-bundle-file={}", isolation::AUTH_BUNDLE_PATH),
            format!("--health-socket-path={HEALTH_SOCKET}"),
        ]
    }

    /// Environment for the supervisor. User-supplied environment travels only
    /// as JSON for mediated workload children, never as supervisor variables.
    fn supervisor_environment(
        &self,
        sandbox: &DriverSandbox,
        plan: &Plan,
        image: &RuntimeConfig,
    ) -> Vec<(String, String)> {
        let spec = sandbox.spec.as_ref();
        let template = spec.and_then(|s| s.template.as_ref());
        let mut env: Vec<(String, String)> = Vec::new();
        let mut set = |key: &str, value: String| {
            env.retain(|(k, _)| k != key);
            env.push((key.to_owned(), value));
        };

        if let Some(level) = spec.map(|s| s.log_level.as_str()).filter(|l| !l.is_empty()) {
            set(sandbox_env::LOG_LEVEL, level.to_owned());
        }
        let mut user_env: std::collections::BTreeMap<String, String> = Default::default();
        if let Some(template) = template {
            user_env.extend(template.environment.clone());
        }
        if let Some(spec) = spec {
            user_env.extend(spec.environment.clone());
        }
        if !user_env.is_empty() {
            set(
                sandbox_env::USER_ENVIRONMENT,
                serde_json::to_string(&user_env).unwrap_or_default(),
            );
        }

        set(sandbox_env::SANDBOX, sandbox.name.clone());
        set(sandbox_env::SANDBOX_ID, sandbox.id.clone());
        set(sandbox_env::ENDPOINT, self.config.grpc_endpoint.clone());
        set(
            sandbox_env::SSH_SOCKET_PATH,
            openshell_core::container_paths::SSH_SOCKET_PATH.to_owned(),
        );
        set("OPENSHELL_CONTAINER_IMAGE", plan.image.clone());
        if let Ok(main) = sandbox_env::MainProcessConfig::encode_driver_spec(spec) {
            set(sandbox_env::MAIN_PROCESS_SPEC, main);
        }
        set(
            sandbox_env::TELEMETRY_ENABLED,
            openshell_core::telemetry::enabled_env_value().to_owned(),
        );
        set(
            sandbox_env::NETWORK_RUNTIME_CAPABILITIES,
            sandbox_env::POLICY_DNS_TRANSPARENT_TCP_CAPABILITY.to_owned(),
        );
        set(
            sandbox_env::ADMITTED_ISOLATION_BACKEND,
            openshell_sandbox_backend::BACKEND_NAME.to_owned(),
        );
        // The supervisor runs as the non-root workload identity, so its
        // generated interception CA goes under its writable /run.
        set(sandbox_env::PROXY_TLS_DIR, PROXY_TLS_DIR.to_owned());
        if self.config.tls_ca.is_some() {
            set(
                sandbox_env::TLS_CA,
                openshell_core::driver_utils::TLS_CA_MOUNT_PATH.to_owned(),
            );
        }
        match image.user.as_deref().filter(|user| !user.is_empty()) {
            Some(user) => {
                set(sandbox_env::OCI_IMAGE_USER, user.to_owned());
                set(sandbox_env::SANDBOX_UID, String::new());
                set(sandbox_env::SANDBOX_GID, String::new());
            }
            None => {
                set(sandbox_env::OCI_IMAGE_USER, String::new());
                set(
                    sandbox_env::SANDBOX_UID,
                    sandbox_env::DEFAULT_SANDBOX_UID.to_string(),
                );
                set(
                    sandbox_env::SANDBOX_GID,
                    sandbox_env::DEFAULT_SANDBOX_GID.to_string(),
                );
            }
        }
        if spec.is_some_and(|s| !s.sandbox_token.is_empty()) {
            set(
                sandbox_env::SANDBOX_TOKEN_FILE,
                openshell_core::driver_utils::SANDBOX_TOKEN_MOUNT_PATH.to_owned(),
            );
        }
        set("PATH", DEFAULT_PATH.to_owned());
        env
    }
}

const DEFAULT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

const ROOT_DIR: FileMeta = FileMeta {
    uid: 0,
    gid: 0,
    mode: 0o755,
};

/// The LXD objects that make up one sandbox.
struct Names {
    workload: String,
    supervisor: String,
    volume: String,
}

impl Names {
    fn new(sandbox_id: &str) -> Self {
        Self {
            workload: instance_name(sandbox_id),
            supervisor: named(SUPERVISOR_PREFIX, sandbox_id),
            volume: named(VOLUME_PREFIX, sandbox_id),
        }
    }
}

/// `raw.lxc` network lines for the workload. Its network namespace has no
/// NIC, but the runtime's socket mediation needs loopback, which LXC leaves
/// down without a network entry. The DNS relay binds 127.0.0.53:53 without
/// capabilities, as OpenShell's Docker and Kubernetes drivers allow; with no
/// NIC that opens nothing beyond loopback.
const WORKLOAD_NETWORK: [&str; 3] = [
    "lxc.net.0.type = empty",
    "lxc.net.0.flags = up",
    "lxc.sysctl.net.ipv4.ip_unprivileged_port_start = 0",
];

/// The supervisor's address on its bridge, which liblxc configures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StaticAddress {
    address: Ipv4Addr,
    prefix: u8,
    gateway: Ipv4Addr,
}

/// `raw.lxc` lines for the supervisor's network: a static address on the
/// bridged NIC, or the host's network namespace.
fn supervisor_network_lxc(address: Option<&StaticAddress>) -> Vec<String> {
    match address {
        Some(address) => vec![
            format!(
                "lxc.net.0.ipv4.address = {}/{}",
                address.address, address.prefix
            ),
            format!("lxc.net.0.ipv4.gateway = {}", address.gateway),
        ],
        None => vec!["lxc.net.0.type = none".to_owned()],
    }
}

/// A host address in `gateway`'s subnet that is not the gateway or in
/// `used`. The search starts at a point derived from `seed`, so concurrent
/// creates rarely race for the same address.
fn pick_address(gateway: Ipv4Addr, prefix: u8, used: &[Ipv4Addr], seed: &str) -> Option<Ipv4Addr> {
    if !(8..=30).contains(&prefix) {
        return None;
    }
    let size = 1u32 << (32 - prefix);
    let network = u32::from(gateway) & !(size - 1);
    // Host numbers run from 1 to size - 2; the ends are the network and
    // broadcast addresses.
    let hosts = size - 2;
    let start = (fnv1a(seed.as_bytes()) % u64::from(hosts)) as u32;
    (0..hosts)
        .map(|offset| Ipv4Addr::from(network + 1 + (start + offset) % hosts))
        .find(|candidate| *candidate != gateway && !used.contains(candidate))
}

/// Inputs for writing the bootstrap material.
struct Stage<'a> {
    generation: &'a str,
    child_env: HashMap<String, String>,
    launch: &'a SandboxLaunchAuthentication,
    host_gateway_ip: IpAddr,
}

/// A validated create request.
struct Plan {
    image: String,
    limits: Vec<(String, String)>,
    gpu: Option<String>,
    labels: HashMap<String, String>,
}

/// LXD instance name for a sandbox's workload: a valid hostname, stable for
/// its ID.
pub fn instance_name(sandbox_id: &str) -> String {
    named(NAME_PREFIX, sandbox_id)
}

fn named(prefix: &str, sandbox_id: &str) -> String {
    let simple = sandbox_id.len() <= 63 - prefix.len()
        && sandbox_id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !sandbox_id.ends_with('-');
    if simple && !sandbox_id.is_empty() {
        format!("{prefix}{sandbox_id}")
    } else {
        format!("{prefix}{:016x}", fnv1a(sandbox_id.as_bytes()))
    }
}

fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

fn role(instance: &Instance) -> Option<&str> {
    if instance.config.get(KEY_MANAGED_BY).map(String::as_str) != Some(MANAGED_BY) {
        return None;
    }
    instance.config.get(KEY_ROLE).map(String::as_str)
}

fn is_true(instance: &Instance, key: &str) -> bool {
    instance.config.get(key).map(String::as_str) == Some("true")
}

fn desired_running(instance: &Instance) -> bool {
    instance.config.get(KEY_DESIRED_STATE).map(String::as_str) == Some("running")
}

/// True when the gateway wants the sandbox running, the workload is up, and
/// the supervisor that enforces its policy is not.
pub fn supervisor_lost(workload: &Instance, supervisor: Option<&Instance>) -> bool {
    workload.status == "Running"
        && desired_running(workload)
        && !is_true(workload, KEY_STARTING)
        && !supervisor.is_some_and(|s| s.status == "Running")
}

/// Names of NIC-like devices LXD will give the instance.
fn network_devices(instance: &Instance) -> Vec<String> {
    let mut devices: Vec<String> = instance
        .expanded_devices
        .iter()
        .chain(instance.devices.iter())
        .filter(|(_, device)| {
            matches!(
                device.get("type").map(String::as_str),
                Some("nic" | "infiniband")
            )
        })
        .map(|(name, _)| name.clone())
        .collect();
    devices.sort();
    devices.dedup();
    devices
}

fn decode_launch_authentication(encoded: &[u8]) -> Result<SandboxLaunchAuthentication> {
    if encoded.is_empty() {
        return Err(DriverError::Precondition(
            "sandbox launch authentication is required".into(),
        ));
    }
    let authentication: SandboxLaunchAuthentication =
        serde_json::from_slice(encoded).map_err(|e| {
            DriverError::Precondition(format!("decode sandbox launch authentication: {e}"))
        })?;
    authentication.validate().map_err(|e| {
        DriverError::Precondition(format!("validate sandbox launch authentication: {e}"))
    })?;
    Ok(authentication)
}

/// Environment for the agent's own processes: image `ENV`, then template,
/// then sandbox. Reserved `OPENSHELL_` names never reach the workload.
fn child_environment(
    sandbox: &DriverSandbox,
    image_env: Option<&[String]>,
) -> HashMap<String, String> {
    let mut environment: HashMap<String, String> = image_env
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.split_once('='))
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect();
    if let Some(spec) = sandbox.spec.as_ref() {
        if let Some(template) = spec.template.as_ref() {
            environment.extend(template.environment.clone());
        }
        environment.extend(spec.environment.clone());
    }
    environment.retain(|key, _| !key.starts_with("OPENSHELL_"));
    environment
}

/// The gateway's view of a managed workload; `None` for anything else.
pub fn driver_sandbox(workload: &Instance, supervisor: Option<&Instance>) -> Option<DriverSandbox> {
    let config = &workload.config;
    role(workload).filter(|role| *role == isolation::ROLE_WORKLOAD)?;
    let id = config.get(KEY_SANDBOX_ID)?.clone();
    let get = |key: &str| config.get(key).cloned().unwrap_or_default();
    let resolved_identity = config.get(KEY_UID).and_then(|uid| {
        Some(StatusIdentity {
            uid: uid.parse().ok()?,
            gid: config.get(KEY_GID)?.parse().ok()?,
            supplementary_gids: get(KEY_GROUPS)
                .split(',')
                .filter_map(|gid| gid.parse().ok())
                .collect(),
            source: get(KEY_IDENTITY_SOURCE),
            resource_digest: get(KEY_IMAGE_DIGEST),
        })
    });
    Some(DriverSandbox {
        id,
        name: get(KEY_SANDBOX_NAME),
        namespace: String::new(),
        spec: None,
        status: Some(DriverSandboxStatus {
            name: workload.name.clone(),
            instance_id: workload.name.clone(),
            conditions: vec![ready_condition(workload, supervisor)],
            deleting: false,
            resolved_identity,
            ..Default::default()
        }),
        workspace: get(KEY_WORKSPACE),
    })
}

fn ready_condition(workload: &Instance, supervisor: Option<&Instance>) -> DriverCondition {
    let supervisor_running = supervisor.is_some_and(|s| s.status == "Running");
    let (status, reason, message) = if is_true(workload, KEY_STARTING) {
        (
            "False",
            CONDITION_STARTING,
            "Sandbox is being provisioned".to_owned(),
        )
    } else {
        match workload.status.as_str() {
            "Running" if supervisor_running => {
                ("True", CONDITION_RUNNING, "Instance is running".to_owned())
            }
            "Running" => (
                "False",
                CONDITION_EXITED,
                "Supervisor is not running; the workload is being stopped".to_owned(),
            ),
            "Stopped" if desired_running(workload) => (
                "False",
                CONDITION_EXITED,
                "Workload exited and the instance stopped".to_owned(),
            ),
            "Stopped" => ("False", CONDITION_STOPPED, "Instance is stopped".to_owned()),
            "Frozen" => ("False", "ContainerFrozen", "Instance is frozen".to_owned()),
            "Error" => (
                "False",
                "ContainerError",
                "LXD reports the instance in error".to_owned(),
            ),
            other => ("Unknown", "Unknown", format!("Instance status is {other}")),
        }
    };
    let transition_time = match workload.status.as_str() {
        "Running" => workload.last_used_at.as_deref(),
        _ => None,
    }
    .and_then(|time| time.parse().ok());
    DriverCondition {
        r#type: "Ready".into(),
        status: status.into(),
        reason: reason.into(),
        message,
        transition_time,
    }
}

/// Resolves the workload's uid, gid and supplementary groups against the
/// image's own account files, never the host's. Policy-requested identity wins
/// over the image `USER`; root and unknown names are refused, and an image
/// with no `USER` gets OpenShell's default non-root identity.
pub fn resolve_identity(
    spec: Option<&DriverSandboxSpec>,
    image_digest: &str,
    image_user: &str,
    passwd: &str,
    group: &str,
) -> Result<Identity> {
    let accounts: Vec<(&str, u32, u32)> = passwd
        .lines()
        .filter_map(|line| {
            let mut fields = line.split(':');
            let name = fields.next()?;
            fields.next()?;
            Some((
                name,
                fields.next()?.parse().ok()?,
                fields.next()?.parse().ok()?,
            ))
        })
        .collect();
    let groups: Vec<(&str, u32, &str)> = group
        .lines()
        .filter_map(|line| {
            let mut fields = line.split(':');
            let name = fields.next()?;
            fields.next()?;
            Some((
                name,
                fields.next()?.parse().ok()?,
                fields.next().unwrap_or(""),
            ))
        })
        .collect();

    let request = spec.and_then(|spec| spec.workload_identity.as_ref());
    let requested_user = request.map_or("", |r| r.user.trim());
    let requested_group = request.map_or("", |r| r.group.trim());
    let (image_user, image_group) = image_user.split_once(':').unwrap_or((image_user, ""));
    let user = if requested_user.is_empty() {
        image_user.trim()
    } else {
        requested_user
    };
    let group_selector = if requested_group.is_empty() {
        image_group.trim()
    } else {
        requested_group
    };
    let invalid = |e: openshell_isolation_interface::contract::BackendError| {
        DriverError::Precondition(e.to_string())
    };
    if user.is_empty() && group_selector.is_empty() {
        // No USER in the image and no policy identity: use OpenShell's default
        // non-root identity, as the Docker, Podman and Kubernetes drivers do.
        return Identity::new(
            sandbox_env::DEFAULT_SANDBOX_UID,
            sandbox_env::DEFAULT_SANDBOX_GID,
            Vec::new(),
            "default".into(),
            image_digest.into(),
        )
        .map_err(invalid);
    }

    let account = accounts
        .iter()
        .find(|(name, uid, _)| *name == user || user.parse::<u32>().ok() == Some(*uid));
    let uid = user
        .parse::<u32>()
        .ok()
        .or_else(|| account.map(|(_, uid, _)| *uid))
        .ok_or_else(|| {
            DriverError::Precondition(format!(
                "user {user:?} is not in the image's /etc/passwd; configure a non-root workload user present in the pinned image"
            ))
        })?;
    let gid = if group_selector.is_empty() {
        account.map(|(_, _, gid)| *gid)
    } else {
        group_selector.parse::<u32>().ok().or_else(|| {
            groups
                .iter()
                .find(|(name, _, _)| *name == group_selector)
                .map(|(_, gid, _)| *gid)
        })
    }
    .ok_or_else(|| {
        DriverError::Precondition(format!(
            "group {group_selector:?} is not in the image's /etc/group; configure an explicit workload group for a UID without an image passwd entry"
        ))
    })?;

    let supplementary: BTreeSet<u32> = account
        .map(|(name, _, _)| {
            groups
                .iter()
                .filter(|(_, id, members)| *id != gid && members.split(',').any(|m| m == *name))
                .map(|(_, gid, _)| *gid)
                .collect()
        })
        .unwrap_or_default();
    let source = if requested_user.is_empty() && requested_group.is_empty() {
        "image"
    } else {
        "policy"
    };
    Identity::new(
        uid,
        gid,
        supplementary.into_iter().collect(),
        source.into(),
        image_digest.into(),
    )
    .map_err(invalid)
}

/// `raw.lxc` that makes `command` LXC's init process under `identity`,
/// followed by the `network` lines.
fn raw_lxc(
    command: &[String],
    identity: &Identity,
    working_dir: &str,
    network: &[String],
) -> Result<String> {
    let mut words = Vec::with_capacity(command.len());
    for arg in command {
        check_init_arg(arg)?;
        words.push(quote_init_arg(arg));
    }
    check_init_arg(working_dir)?;
    let mut lines = vec![
        format!("lxc.init.cmd = {}", words.join(" ")),
        format!("lxc.init.uid = {}", identity.uid),
        format!("lxc.init.gid = {}", identity.gid),
        format!("lxc.init.cwd = {working_dir}"),
        "lxc.no_new_privs = 1".to_owned(),
        // Dropping to a non-root uid clears the effective capabilities but
        // not the bounding set, which OpenShell's runtime requires empty.
        "lxc.cap.keep = none".to_owned(),
    ];
    if !identity.supplementary_gids.is_empty() {
        lines.push(format!(
            "lxc.init.groups = {}",
            join_ids(&identity.supplementary_gids)
        ));
    }
    lines.extend_from_slice(network);
    Ok(lines.join("\n"))
}

/// `lxc.init.cmd` is one config line that LXC splits on whitespace, honouring
/// double quotes; arguments that cannot survive that round trip are refused.
fn check_init_arg(arg: &str) -> Result<()> {
    if arg.contains(['\n', '\r', '"', '\0']) {
        return Err(DriverError::InvalidArgument(format!(
            "command argument {arg:?} contains characters the LXD driver cannot pass to init"
        )));
    }
    Ok(())
}

fn quote_init_arg(arg: &str) -> String {
    if arg.is_empty() || arg.contains(char::is_whitespace) || arg.contains('\'') {
        format!("\"{arg}\"")
    } else {
        arg.to_owned()
    }
}

fn join_ids(ids: &[u32]) -> String {
    ids.iter().map(u32::to_string).collect::<Vec<_>>().join(",")
}

fn valid_env_key(key: &str) -> bool {
    let mut bytes = key.bytes();
    matches!(bytes.next(), Some(b'A'..=b'Z' | b'a'..=b'z' | b'_'))
        && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

#[cfg(test)]
mod tests {
    use super::*;

    const PASSWD: &str =
        "root:x:0:0:root:/root:/bin/bash\nagent:x:1000:1000::/home/agent:/bin/sh\n";
    const GROUP: &str = "root:x:0:\nagent:x:1000:\nvideo:x:44:agent\nwheel:x:10:root\n";

    fn spec_with_identity(user: &str, group: &str) -> DriverSandboxSpec {
        DriverSandboxSpec {
            workload_identity: Some(crate::proto::compute::v1::WorkloadIdentityRequest {
                user: user.into(),
                group: group.into(),
            }),
            ..Default::default()
        }
    }

    fn workload(status: &str, desired: &str) -> Instance {
        Instance {
            name: "openshell-x".into(),
            status: status.into(),
            config: HashMap::from([
                (KEY_MANAGED_BY.into(), MANAGED_BY.into()),
                (KEY_ROLE.into(), isolation::ROLE_WORKLOAD.into()),
                (KEY_SANDBOX_ID.into(), "x".into()),
                (KEY_DESIRED_STATE.into(), desired.into()),
            ]),
            created_at: None,
            last_used_at: Some("2026-10-22T10:00:00Z".into()),
            profiles: vec![],
            devices: HashMap::new(),
            expanded_devices: HashMap::new(),
        }
    }

    fn supervisor(status: &str) -> Instance {
        let mut instance = workload(status, "running");
        instance.name = "openshell-sup-x".into();
        instance
            .config
            .insert(KEY_ROLE.into(), isolation::ROLE_SUPERVISOR.into());
        instance
    }

    #[test]
    fn names_instances_after_sandbox_ids() {
        assert_eq!(
            instance_name("3f2a9c1e-0000-4000-8000-1234567890ab"),
            "openshell-3f2a9c1e-0000-4000-8000-1234567890ab"
        );
        let hashed = instance_name("Not_A/Hostname");
        assert!(hashed.starts_with("openshell-") && hashed.len() == 26);
        assert_eq!(hashed, instance_name("Not_A/Hostname"));
        assert!(instance_name(&"a".repeat(80)).len() <= 63);

        let names = Names::new("3f2a9c1e-0000-4000-8000-1234567890ab");
        assert_eq!(
            names.supervisor,
            "openshell-sup-3f2a9c1e-0000-4000-8000-1234567890ab"
        );
        assert!(names.supervisor.len() <= 63);
        assert!(Names::new(&"b".repeat(80)).supervisor.len() <= 63);
    }

    #[test]
    fn resolves_image_user_and_groups() {
        let identity = resolve_identity(None, "sha256:i", "agent", PASSWD, GROUP).unwrap();
        assert_eq!((identity.uid, identity.gid), (1000, 1000));
        assert_eq!(identity.supplementary_gids, vec![44]);
        assert_eq!(identity.source, "image");
        assert_eq!(identity.resource_digest, "sha256:i");
    }

    #[test]
    fn policy_identity_overrides_image() {
        let spec = spec_with_identity("1000", "video");
        let identity = resolve_identity(Some(&spec), "sha256:i", "nobody", PASSWD, GROUP).unwrap();
        assert_eq!((identity.uid, identity.gid), (1000, 44));
        assert_eq!(identity.source, "policy");
    }

    #[test]
    fn images_without_a_user_get_the_default_identity() {
        let identity = resolve_identity(None, "sha256:i", "", PASSWD, GROUP).unwrap();
        assert_eq!(
            (identity.uid, identity.gid, identity.source.as_str()),
            (1000, 1000, "default")
        );
    }

    #[test]
    fn refuses_root_and_unknown_users() {
        for (user, group) in [("root", ""), ("ghost", ""), ("agent", "wheel-less")] {
            let spec = spec_with_identity(user, group);
            assert!(resolve_identity(Some(&spec), "sha256:i", "", PASSWD, GROUP).is_err());
        }
    }

    #[test]
    fn builds_init_config() {
        let identity = resolve_identity(None, "sha256:i", "agent", PASSWD, GROUP).unwrap();
        let raw = raw_lxc(
            &["/bin/sh".into(), "-c".into(), "echo hi there".into()],
            &identity,
            "/work",
            &[],
        )
        .unwrap();
        assert_eq!(
            raw,
            "lxc.init.cmd = /bin/sh -c \"echo hi there\"\nlxc.init.uid = 1000\nlxc.init.gid = 1000\nlxc.init.cwd = /work\nlxc.no_new_privs = 1\nlxc.cap.keep = none\nlxc.init.groups = 44"
        );
        assert!(raw_lxc(&["say \"hi\"".into()], &identity, "/", &[]).is_err());
        assert!(
            raw_lxc(
                &["/x".into()],
                &identity,
                "/",
                &supervisor_network_lxc(None)
            )
            .unwrap()
            .ends_with("lxc.net.0.type = none")
        );
        let address = StaticAddress {
            address: Ipv4Addr::new(10, 0, 3, 7),
            prefix: 24,
            gateway: Ipv4Addr::new(10, 0, 3, 1),
        };
        assert!(
            raw_lxc(
                &["/x".into()],
                &identity,
                "/",
                &supervisor_network_lxc(Some(&address))
            )
            .unwrap()
            .ends_with("lxc.net.0.ipv4.address = 10.0.3.7/24\nlxc.net.0.ipv4.gateway = 10.0.3.1")
        );
    }

    #[test]
    fn picks_free_bridge_addresses() {
        let gateway = Ipv4Addr::new(10, 0, 3, 1);
        let address = pick_address(gateway, 24, &[], "sbx-1").unwrap();
        assert_eq!(address.octets()[..3], [10, 0, 3]);
        assert!(!matches!(address.octets()[3], 0 | 1 | 255));
        // Stable for a sandbox, and skips taken addresses.
        assert_eq!(pick_address(gateway, 24, &[], "sbx-1"), Some(address));
        assert_ne!(
            pick_address(gateway, 24, &[address], "sbx-1"),
            Some(address)
        );
        // A /30 has two hosts: the gateway and one more.
        assert_eq!(
            pick_address(gateway, 30, &[], "x"),
            Some(Ipv4Addr::new(10, 0, 3, 2))
        );
        assert_eq!(
            pick_address(gateway, 30, &[Ipv4Addr::new(10, 0, 3, 2)], "x"),
            None
        );
        assert_eq!(pick_address(gateway, 31, &[], "x"), None);
    }

    #[test]
    fn ready_requires_both_containers() {
        let condition = |w: &Instance, s: Option<&Instance>| {
            driver_sandbox(w, s).unwrap().status.unwrap().conditions[0].clone()
        };
        let running = workload("Running", "running");
        let up = condition(&running, Some(&supervisor("Running")));
        assert_eq!(
            (up.status.as_str(), up.reason.as_str()),
            ("True", CONDITION_RUNNING)
        );
        assert!(up.transition_time.is_some());

        let lost = condition(&running, Some(&supervisor("Stopped")));
        assert_eq!(
            (lost.status.as_str(), lost.reason.as_str()),
            ("False", CONDITION_EXITED)
        );
        assert_eq!(condition(&running, None).status, "False");

        let stopped = workload("Stopped", "running");
        assert_eq!(condition(&stopped, None).reason, CONDITION_EXITED);
        let stopped = workload("Stopped", "stopped");
        assert_eq!(condition(&stopped, None).reason, CONDITION_STOPPED);

        // A pair still being brought up is neither ready nor failed.
        let mut starting = workload("Running", "stopped");
        starting.config.insert(KEY_STARTING.into(), "true".into());
        assert_eq!(condition(&starting, None).reason, CONDITION_STARTING);

        // Other instances and supervisors are not sandboxes.
        assert!(driver_sandbox(&supervisor("Running"), None).is_none());
        let mut foreign = workload("Running", "running");
        foreign.config.remove(KEY_MANAGED_BY);
        assert!(driver_sandbox(&foreign, None).is_none());
    }

    #[test]
    fn fails_closed_only_when_a_running_pair_loses_its_supervisor() {
        let running = workload("Running", "running");
        assert!(supervisor_lost(&running, None));
        assert!(supervisor_lost(&running, Some(&supervisor("Stopped"))));
        assert!(!supervisor_lost(&running, Some(&supervisor("Running"))));
        // Gateway-requested stop, or a pair that is still starting.
        assert!(!supervisor_lost(&workload("Running", "stopped"), None));
        assert!(!supervisor_lost(&workload("Stopped", "running"), None));
        let mut starting = workload("Running", "running");
        starting.config.insert(KEY_STARTING.into(), "true".into());
        assert!(!supervisor_lost(&starting, None));
    }

    #[test]
    fn spots_network_devices() {
        let mut instance = workload("Stopped", "stopped");
        assert!(network_devices(&instance).is_empty());
        instance.expanded_devices.insert(
            "eth0".into(),
            HashMap::from([("type".into(), "nic".into())]),
        );
        instance.expanded_devices.insert(
            "root".into(),
            HashMap::from([("type".into(), "disk".into())]),
        );
        assert_eq!(network_devices(&instance), vec!["eth0"]);
    }

    #[test]
    fn child_environment_drops_reserved_names() {
        let sandbox = DriverSandbox {
            spec: Some(DriverSandboxSpec {
                environment: HashMap::from([
                    ("LANG".to_owned(), "C.UTF-8".to_owned()),
                    ("OPENSHELL_ENDPOINT".to_owned(), "evil".to_owned()),
                ]),
                ..Default::default()
            }),
            ..Default::default()
        };
        let image = vec!["LANG=C".to_owned(), "HOME=/home/agent".to_owned()];
        let env = child_environment(&sandbox, Some(&image));
        assert_eq!(env["LANG"], "C.UTF-8");
        assert_eq!(env["HOME"], "/home/agent");
        assert!(!env.contains_key("OPENSHELL_ENDPOINT"));
    }
}
