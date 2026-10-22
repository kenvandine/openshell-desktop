//! Sandbox lifecycle on LXD.
//!
//! Each sandbox is one unprivileged LXD container named after the gateway's
//! sandbox ID. It is created with no profiles, so it gets a root disk and no
//! network device at all: the workload has no route out, which is the same
//! outer fence the Docker and Podman drivers get from `network_mode=none`.
//! The OCI entrypoint runs as LXC's init process under the resolved non-root
//! identity with `no_new_privs`.
//!
//! OpenShell's supervisor companion (policy engine, egress proxy, gateway
//! session) is not provisioned yet; see `docs/lxd-driver.md`.

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::broadcast;
use tracing::{debug, info, warn};

use crate::admission::AdmissionConfig;
use crate::lxd::{self, Instance, LxdError};
use crate::oci::{ImageError, ImportedImage, Importer, RuntimeConfig};
use crate::proto::compute::v1::{
    DriverCondition, DriverSandbox, DriverSandboxStatus, ResolvedWorkloadIdentity,
    WatchSandboxesDeletedEvent, WatchSandboxesEvent, WatchSandboxesSandboxEvent,
    watch_sandboxes_event,
};
use crate::quantity;

pub const DEFAULT_IMAGE: &str = "nvcr.io/nvidia/base/ubuntu:24.04";

/// Instance name prefix; also how the event pump spots our instances.
const NAME_PREFIX: &str = "openshell-";

const KEY_MANAGED_BY: &str = "user.openshell.managed-by";
const KEY_SANDBOX_ID: &str = "user.openshell.sandbox-id";
const KEY_SANDBOX_NAME: &str = "user.openshell.sandbox-name";
const KEY_WORKSPACE: &str = "user.openshell.workspace";
const KEY_LABELS: &str = "user.openshell.labels";
const KEY_IMAGE: &str = "user.openshell.image";
const KEY_IMAGE_DIGEST: &str = "user.openshell.image-digest";
/// `running` or `stopped`: what the gateway last asked for. Tells a stop the
/// gateway requested apart from the workload exiting on its own.
const KEY_DESIRED_STATE: &str = "user.openshell.desired-state";
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

const DEFAULT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

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
/// production; tests substitute a fixed image.
#[tonic::async_trait]
pub trait ImageSource: Send + Sync {
    async fn ensure(
        &self,
        lxd: &lxd::Client,
        reference: &str,
    ) -> std::result::Result<ImportedImage, ImageError>;
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
        }
    }
}

#[derive(Clone)]
pub struct LxdDriver {
    lxd: lxd::Client,
    images: Arc<dyn ImageSource>,
    config: Arc<DriverConfig>,
    events: broadcast::Sender<WatchSandboxesEvent>,
    /// Instance name → sandbox ID, so a deletion event (which only carries
    /// the instance name) can be reported by sandbox ID.
    known: Arc<Mutex<HashMap<String, String>>>,
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
        let name = instance_name(&sandbox.id);
        if let Some(existing) = self.lxd.instance(&name).await? {
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

        let image = self.images.ensure(&self.lxd, &plan.image).await?;
        info!(sandbox_id = %sandbox.id, instance = %name, image = %plan.image, digest = %image.manifest_digest, "creating sandbox");
        self.lxd
            .create_instance(&self.instance_body(&name, sandbox, &plan, &image))
            .await?;
        self.known
            .lock()
            .unwrap()
            .insert(name.clone(), sandbox.id.clone());

        // Everything after the instance exists must clean it up on failure.
        if let Err(error) = self.finish_create(&name, sandbox, &plan, &image).await {
            warn!(instance = %name, %error, "sandbox create failed; removing instance");
            if let Err(cleanup) = self.lxd.delete_instance(&name).await {
                warn!(instance = %name, error = %cleanup, "could not remove failed instance");
            }
            self.known.lock().unwrap().remove(&name);
            return Err(error);
        }
        Ok(())
    }

    /// Resolves the workload identity from the image's account files, then
    /// sets the init process and starts the instance.
    async fn finish_create(
        &self,
        name: &str,
        sandbox: &DriverSandbox,
        plan: &Plan,
        image: &ImportedImage,
    ) -> Result<()> {
        let passwd = self
            .lxd
            .read_file(name, "/etc/passwd")
            .await?
            .unwrap_or_default();
        let group = self
            .lxd
            .read_file(name, "/etc/group")
            .await?
            .unwrap_or_default();
        let request = sandbox
            .spec
            .as_ref()
            .and_then(|spec| spec.workload_identity.as_ref());
        let identity = resolve_identity(
            request.map_or("", |r| r.user.as_str()),
            request.map_or("", |r| r.group.as_str()),
            image.config.user.as_deref().unwrap_or(""),
            &String::from_utf8_lossy(&passwd),
            &String::from_utf8_lossy(&group),
        )?;

        let command = plan.command(&image.config);
        if command.is_empty() {
            return Err(DriverError::InvalidArgument(
                "sandbox has no command and the image has no entrypoint".into(),
            ));
        }
        let working_dir = image
            .config
            .working_dir
            .as_deref()
            .filter(|dir| dir.starts_with('/'))
            .unwrap_or("/");

        let mut config = HashMap::new();
        config.insert(
            "raw.lxc".to_owned(),
            raw_lxc(&command, &identity, working_dir)?,
        );
        for (key, value) in plan.environment(&image.config) {
            config.insert(format!("environment.{key}"), value);
        }
        config.insert(KEY_UID.into(), identity.uid.to_string());
        config.insert(KEY_GID.into(), identity.gid.to_string());
        config.insert(KEY_GROUPS.into(), join_ids(&identity.supplementary_gids));
        config.insert(KEY_IDENTITY_SOURCE.into(), identity.source.clone());
        config.insert(KEY_DESIRED_STATE.into(), "running".into());
        self.lxd.patch_instance_config(name, &config).await?;
        self.lxd.set_state(name, "start", false, -1).await?;
        Ok(())
    }

    pub async fn start(&self, sandbox_id: &str) -> Result<()> {
        let instance = self.managed(sandbox_id).await?;
        self.set_desired(&instance.name, "running").await?;
        if instance.status != "Running" {
            self.lxd
                .set_state(&instance.name, "start", false, -1)
                .await?;
        }
        Ok(())
    }

    pub async fn stop(&self, sandbox_id: &str) -> Result<()> {
        let instance = self.managed(sandbox_id).await?;
        self.set_desired(&instance.name, "stopped").await?;
        self.stop_instance(&instance).await
    }

    /// Returns whether an instance was deleted.
    pub async fn delete(&self, sandbox_id: &str) -> Result<bool> {
        let instance = match self.managed(sandbox_id).await {
            Ok(instance) => instance,
            Err(DriverError::NotFound(_)) => return Ok(false),
            Err(error) => return Err(error),
        };
        self.set_desired(&instance.name, "stopped").await?;
        self.stop_instance(&instance).await?;
        match self.lxd.delete_instance(&instance.name).await {
            Ok(()) | Err(LxdError::NotFound(_)) => Ok(true),
            Err(error) => Err(error.into()),
        }
    }

    pub async fn get(&self, sandbox_id: &str) -> Result<Option<DriverSandbox>> {
        match self.managed(sandbox_id).await {
            Ok(instance) => Ok(self.observe(&instance)),
            Err(DriverError::NotFound(_)) => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub async fn list(&self) -> Result<Vec<DriverSandbox>> {
        let instances = self.lxd.instances().await?;
        Ok(instances.iter().filter_map(|i| self.observe(i)).collect())
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
        for instance in &instances {
            if let Some(sandbox) = self.observe(instance) {
                self.publish(sandbox);
            }
        }
    }

    async fn refresh(&self, name: &str) {
        match self.lxd.instance(name).await {
            Ok(Some(instance)) => {
                if let Some(sandbox) = self.observe(&instance) {
                    self.publish(sandbox);
                }
            }
            Ok(None) => self.publish_deleted(name),
            Err(error) => warn!(instance = %name, %error, "could not read instance after event"),
        }
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

    /// Turns a managed instance into the gateway's view of a sandbox, and
    /// remembers it for deletion events. `None` for other instances.
    fn observe(&self, instance: &Instance) -> Option<DriverSandbox> {
        let sandbox = driver_sandbox(instance)?;
        self.known
            .lock()
            .unwrap()
            .insert(instance.name.clone(), sandbox.id.clone());
        Some(sandbox)
    }

    async fn managed(&self, sandbox_id: &str) -> Result<Instance> {
        if sandbox_id.is_empty() {
            return Err(DriverError::InvalidArgument(
                "sandbox_id is required".into(),
            ));
        }
        let name = instance_name(sandbox_id);
        match self.lxd.instance(&name).await? {
            Some(instance)
                if instance.config.get(KEY_SANDBOX_ID).map(String::as_str) == Some(sandbox_id) =>
            {
                Ok(instance)
            }
            _ => Err(DriverError::NotFound(format!(
                "sandbox {sandbox_id} not found"
            ))),
        }
    }

    async fn set_desired(&self, name: &str, state: &str) -> Result<()> {
        let config = HashMap::from([(KEY_DESIRED_STATE.to_owned(), state.to_owned())]);
        Ok(self.lxd.patch_instance_config(name, &config).await?)
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

        let mut environment = Vec::new();
        for (key, value) in template.environment.iter().chain(spec.environment.iter()) {
            if !valid_env_key(key) {
                return Err(DriverError::InvalidArgument(format!(
                    "invalid environment variable name {key:?}"
                )));
            }
            environment.push((key.clone(), value.clone()));
        }
        for arg in &spec.command {
            check_init_arg(arg)?;
        }

        Ok(Plan {
            image,
            limits,
            gpu,
            environment,
            command: spec.command.clone(),
            labels: template.labels.clone(),
        })
    }

    fn instance_body(
        &self,
        name: &str,
        sandbox: &DriverSandbox,
        plan: &Plan,
        image: &ImportedImage,
    ) -> Value {
        let mut config = serde_json::Map::new();
        let mut set = |key: &str, value: String| {
            config.insert(key.to_owned(), Value::String(value));
        };
        set(KEY_MANAGED_BY, MANAGED_BY.into());
        set(KEY_SANDBOX_ID, sandbox.id.clone());
        set(KEY_SANDBOX_NAME, sandbox.name.clone());
        set(KEY_WORKSPACE, sandbox.workspace.clone());
        set(KEY_IMAGE, plan.image.clone());
        set(KEY_IMAGE_DIGEST, image.manifest_digest.clone());
        set(KEY_DESIRED_STATE, "stopped".into());
        if !plan.labels.is_empty() {
            set(
                KEY_LABELS,
                serde_json::to_string(&plan.labels).unwrap_or_default(),
            );
        }
        set("security.privileged", "false".into());
        set("security.nesting", "false".into());
        for (key, value) in &plan.limits {
            set(key, value.clone());
        }

        let mut devices = json!({
            "root": { "type": "disk", "path": "/", "pool": self.config.storage_pool },
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
            "name": name,
            "type": "container",
            // No profiles: no inherited NIC, so the workload has no network.
            "profiles": [],
            "source": { "type": "image", "alias": image.alias },
            "config": config,
            "devices": devices,
            "start": false,
        })
    }
}

/// A validated create request.
struct Plan {
    image: String,
    limits: Vec<(String, String)>,
    gpu: Option<String>,
    environment: Vec<(String, String)>,
    command: Vec<String>,
    labels: HashMap<String, String>,
}

impl Plan {
    /// The canonical command if the gateway sent one, else the image's
    /// entrypoint followed by its default arguments.
    fn command(&self, image: &RuntimeConfig) -> Vec<String> {
        if !self.command.is_empty() {
            return self.command.clone();
        }
        image
            .entrypoint
            .iter()
            .flatten()
            .chain(image.cmd.iter().flatten())
            .cloned()
            .collect()
    }

    /// Image environment, overridden by the template and then the sandbox.
    fn environment(&self, image: &RuntimeConfig) -> Vec<(String, String)> {
        let mut merged: Vec<(String, String)> = Vec::new();
        let mut put = |key: &str, value: &str| {
            if let Some(slot) = merged.iter_mut().find(|(k, _)| k == key) {
                slot.1 = value.to_owned();
            } else {
                merged.push((key.to_owned(), value.to_owned()));
            }
        };
        for entry in image.env.iter().flatten() {
            if let Some((key, value)) = entry.split_once('=')
                && valid_env_key(key)
            {
                put(key, value);
            }
        }
        for (key, value) in &self.environment {
            put(key, value);
        }
        if !merged.iter().any(|(k, _)| k == "PATH") {
            merged.push(("PATH".into(), DEFAULT_PATH.into()));
        }
        merged
    }
}

/// LXD instance name for a sandbox: a valid hostname, stable for its ID.
pub fn instance_name(sandbox_id: &str) -> String {
    let simple = sandbox_id.len() <= 63 - NAME_PREFIX.len()
        && sandbox_id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !sandbox_id.ends_with('-');
    if simple && !sandbox_id.is_empty() {
        format!("{NAME_PREFIX}{sandbox_id}")
    } else {
        format!("{NAME_PREFIX}{:016x}", fnv1a(sandbox_id.as_bytes()))
    }
}

fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// The gateway's view of a managed instance; `None` for anything else.
pub fn driver_sandbox(instance: &Instance) -> Option<DriverSandbox> {
    let config = &instance.config;
    if config.get(KEY_MANAGED_BY).map(String::as_str) != Some(MANAGED_BY) {
        return None;
    }
    let id = config.get(KEY_SANDBOX_ID)?.clone();
    let get = |key: &str| config.get(key).cloned().unwrap_or_default();
    let resolved_identity = config.get(KEY_UID).and_then(|uid| {
        Some(ResolvedWorkloadIdentity {
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
            name: instance.name.clone(),
            instance_id: instance.name.clone(),
            conditions: vec![ready_condition(instance)],
            deleting: false,
            resolved_identity,
            ..Default::default()
        }),
        workspace: get(KEY_WORKSPACE),
    })
}

fn ready_condition(instance: &Instance) -> DriverCondition {
    let desired = instance.config.get(KEY_DESIRED_STATE).map(String::as_str);
    let (status, reason, message) = match instance.status.as_str() {
        "Running" => ("True", CONDITION_RUNNING, "Instance is running".to_owned()),
        "Stopped" if desired == Some("running") => (
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
    };
    let transition_time = match instance.status.as_str() {
        "Running" => instance.last_used_at.as_deref(),
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
/// image's own account files. Policy-requested identity wins over the image
/// `USER`; root and unknown names are refused, as in OpenShell's drivers.
pub fn resolve_identity(
    requested_user: &str,
    requested_group: &str,
    image_user: &str,
    passwd: &str,
    group: &str,
) -> Result<ResolvedWorkloadIdentity> {
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
    let groups: Vec<(&str, u32, Vec<&str>)> = group
        .lines()
        .filter_map(|line| {
            let mut fields = line.split(':');
            let name = fields.next()?;
            fields.next()?;
            let gid = fields.next()?.parse().ok()?;
            let members = fields
                .next()
                .unwrap_or_default()
                .split(',')
                .filter(|m| !m.is_empty())
                .collect();
            Some((name, gid, members))
        })
        .collect();

    let (image_user, image_group) = image_user.split_once(':').unwrap_or((image_user, ""));
    let (user, source) = match requested_user.trim() {
        "" => (image_user.trim(), "image"),
        user => (user, "policy"),
    };
    let group_selector = match requested_group.trim() {
        "" => image_group.trim(),
        group => group,
    };
    if user.is_empty() {
        return Err(DriverError::Precondition(
            "the image runs as root; set USER in the image or a policy workload identity".into(),
        ));
    }

    let account = accounts.iter().find(|(name, _, _)| *name == user);
    let uid = match user.parse::<u32>() {
        Ok(uid) => uid,
        Err(_) => account.map(|(_, uid, _)| *uid).ok_or_else(|| {
            DriverError::Precondition(format!("user {user:?} is not in the image's /etc/passwd"))
        })?,
    };
    let account = account.or_else(|| accounts.iter().find(|(_, id, _)| *id == uid));
    let gid = if group_selector.is_empty() {
        account.map(|(_, _, gid)| *gid).unwrap_or(uid)
    } else {
        match group_selector.parse::<u32>() {
            Ok(gid) => gid,
            Err(_) => groups
                .iter()
                .find(|(name, _, _)| *name == group_selector)
                .map(|(_, gid, _)| *gid)
                .ok_or_else(|| {
                    DriverError::Precondition(format!(
                        "group {group_selector:?} is not in the image's /etc/group"
                    ))
                })?,
        }
    };
    if uid == 0 || gid == 0 {
        return Err(DriverError::Precondition(
            "sandbox workloads must not run with uid or gid 0".into(),
        ));
    }

    let supplementary_gids: BTreeSet<u32> = account
        .map(|(name, _, _)| {
            groups
                .iter()
                .filter(|(_, gid, members)| *gid != 0 && members.contains(name))
                .map(|(_, gid, _)| *gid)
                .collect()
        })
        .unwrap_or_default();
    Ok(ResolvedWorkloadIdentity {
        uid,
        gid,
        supplementary_gids: supplementary_gids
            .into_iter()
            .filter(|g| *g != gid)
            .collect(),
        source: source.into(),
        resource_digest: String::new(),
    })
}

/// `raw.lxc` that makes the workload command LXC's init process.
fn raw_lxc(
    command: &[String],
    identity: &ResolvedWorkloadIdentity,
    working_dir: &str,
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
    ];
    if !identity.supplementary_gids.is_empty() {
        lines.push(format!(
            "lxc.init.groups = {}",
            join_ids(&identity.supplementary_gids)
        ));
    }
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
    }

    #[test]
    fn resolves_image_user_and_groups() {
        let identity = resolve_identity("", "", "agent", PASSWD, GROUP).unwrap();
        assert_eq!((identity.uid, identity.gid), (1000, 1000));
        assert_eq!(identity.supplementary_gids, vec![44]);
        assert_eq!(identity.source, "image");
    }

    #[test]
    fn policy_identity_overrides_image() {
        let identity = resolve_identity("1000", "video", "nobody", PASSWD, GROUP).unwrap();
        assert_eq!((identity.uid, identity.gid), (1000, 44));
        assert_eq!(identity.source, "policy");
    }

    #[test]
    fn refuses_root_and_unknown_users() {
        assert!(resolve_identity("", "", "", PASSWD, GROUP).is_err());
        assert!(resolve_identity("root", "", "", PASSWD, GROUP).is_err());
        assert!(resolve_identity("ghost", "", "", PASSWD, GROUP).is_err());
        assert!(resolve_identity("agent", "wheel-less", "", PASSWD, GROUP).is_err());
    }

    #[test]
    fn builds_init_config() {
        let identity = resolve_identity("agent", "", "", PASSWD, GROUP).unwrap();
        let raw = raw_lxc(
            &["/bin/sh".into(), "-c".into(), "echo hi there".into()],
            &identity,
            "/work",
        )
        .unwrap();
        assert_eq!(
            raw,
            "lxc.init.cmd = /bin/sh -c \"echo hi there\"\nlxc.init.uid = 1000\nlxc.init.gid = 1000\nlxc.init.cwd = /work\nlxc.no_new_privs = 1\nlxc.init.groups = 44"
        );
        assert!(raw_lxc(&["say \"hi\"".into()], &identity, "/").is_err());
    }

    #[test]
    fn maps_status_to_ready_condition() {
        let mut instance = Instance {
            name: "openshell-x".into(),
            status: "Running".into(),
            config: HashMap::from([
                (KEY_MANAGED_BY.into(), MANAGED_BY.into()),
                (KEY_SANDBOX_ID.into(), "x".into()),
                (KEY_DESIRED_STATE.into(), "running".into()),
            ]),
            created_at: None,
            last_used_at: Some("2026-10-22T10:00:00Z".into()),
        };
        let condition =
            |i: &Instance| driver_sandbox(i).unwrap().status.unwrap().conditions[0].clone();
        assert_eq!(condition(&instance).status, "True");
        assert!(condition(&instance).transition_time.is_some());

        instance.status = "Stopped".into();
        assert_eq!(condition(&instance).reason, CONDITION_EXITED);
        instance
            .config
            .insert(KEY_DESIRED_STATE.into(), "stopped".into());
        assert_eq!(condition(&instance).reason, CONDITION_STOPPED);

        instance.config.remove(KEY_MANAGED_BY);
        assert!(driver_sandbox(&instance).is_none());
    }

    #[test]
    fn merges_environment() {
        let plan = Plan {
            image: String::new(),
            limits: vec![],
            gpu: None,
            environment: vec![("LANG".into(), "C.UTF-8".into())],
            command: vec![],
            labels: HashMap::new(),
        };
        let image = RuntimeConfig {
            env: Some(vec!["LANG=C".into(), "HOME=/home/agent".into()]),
            entrypoint: Some(vec!["/bin/agent".into()]),
            cmd: Some(vec!["--serve".into()]),
            ..Default::default()
        };
        let env = plan.environment(&image);
        assert!(env.contains(&("LANG".into(), "C.UTF-8".into())));
        assert!(env.contains(&("HOME".into(), "/home/agent".into())));
        assert!(env.iter().any(|(k, _)| k == "PATH"));
        assert_eq!(plan.command(&image), vec!["/bin/agent", "--serve"]);
    }
}
