//! Bootstrap material for the workload and its supervisor companion.
//!
//! OpenShell isolates a sandbox with two containers: the workload runs the
//! `openshell-sandbox` runtime as PID 1 with no network, and a supervisor
//! companion holds the gateway session, policy engine and egress proxy. They
//! meet over a Unix socket on a shared volume, authenticated with per-sandbox
//! mTLS. This module builds the files each side needs, following
//! `openshell-driver-podman`'s `isolation.rs`; the driver pushes them into the
//! stopped instances through LXD's file API.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use openshell_core::jwt::SandboxLaunchAuthentication;
use openshell_isolation_interface::contract::{ProvisioningEvidence, ResolvedWorkloadIdentity};
use openshell_sandbox_backend::boundary_protocol::{
    BoundaryConfig, BoundaryListener, GatewayVerificationKey, SandboxRuntimeDescriptor,
    SandboxTlsClientConfig, SandboxTlsServerConfig, SandboxTransport,
    generate_sandbox_tls_material,
};
use serde::{Deserialize, Serialize};

use crate::driver::DriverError;
use crate::lxd::FileMeta;

pub const CHANNEL_ROOT: &str = "/.openshell/channel";
pub const BOOTSTRAP_PATH: &str = "/.openshell/channel/sandbox/bootstrap.json";
/// Where the shared control volume is mounted in both instances. LXD will
/// not change the owner of a mountpoint through its file API, so the socket
/// lives in a directory the driver creates inside it.
pub const CONTROL_VOLUME_ROOT: &str = "/.openshell/channel/shared";
/// The supervisor binds the socket here and the workload runtime dials it.
pub const CONTROL_ROOT: &str = "/.openshell/channel/shared/control";
pub const RUNTIME_DESCRIPTOR_PATH: &str = "/.openshell/supervisor/runtime-descriptor.json";
pub const AUTH_BUNDLE_PATH: &str = "/.openshell/supervisor/auth.json";
pub const RESTART_METADATA_PATH: &str = "/.openshell/supervisor/restart-metadata.json";
const CONTROL_SOCKET: &str = "/.openshell/channel/shared/control/control.sock";
const SANDBOX_DIR: &str = "/.openshell/channel/sandbox";

pub const ROLE_WORKLOAD: &str = "workload";
pub const ROLE_SUPERVISOR: &str = "supervisor";

const ROOT_DIR: FileMeta = FileMeta {
    uid: 0,
    gid: 0,
    mode: 0o755,
};

/// A file or directory to create inside an instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub path: String,
    pub content: Option<Vec<u8>>,
    pub meta: FileMeta,
}

impl Entry {
    fn dir(path: &str, meta: FileMeta) -> Self {
        Self {
            path: path.into(),
            content: None,
            meta,
        }
    }

    fn file(path: &str, content: Vec<u8>, meta: FileMeta) -> Self {
        Self {
            path: path.into(),
            content: Some(content),
            meta,
        }
    }
}

/// What the driver stores on the supervisor so a restart can rebuild the
/// bootstrap material without the original create request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestartMetadata {
    pub generation: String,
    pub workload_identity: ResolvedWorkloadIdentity,
    pub child_env: HashMap<String, String>,
}

pub struct BootstrapInput<'a> {
    pub sandbox_id: &'a str,
    /// LXD name of the workload instance; evidence is bound to it.
    pub instance: &'a str,
    pub generation: &'a str,
    pub host_gateway_ip: std::net::IpAddr,
    pub identity: &'a ResolvedWorkloadIdentity,
    pub child_env: HashMap<String, String>,
    pub launch_authentication: &'a SandboxLaunchAuthentication,
}

pub struct Bootstrap {
    /// Directories and files for the workload's root filesystem.
    pub workload: Vec<Entry>,
    /// Directories and files for the supervisor's root filesystem.
    pub supervisor: Vec<Entry>,
}

/// Facts about the workload's outer network fence, hashed into the
/// provisioning evidence the runtime later confirms.
#[derive(Serialize)]
struct LxdOuterFenceEvidence<'a> {
    instance: &'a str,
    /// Always empty: the workload instance is created with no profiles and
    /// no NIC, so it only has a loopback interface.
    network_devices: &'a [String],
}

fn invalid(error: impl std::fmt::Display) -> DriverError {
    DriverError::Precondition(error.to_string())
}

/// Evidence that `instance` has no network device. `nics` lists every `nic`
/// style device LXD reports on it; any entry fails the fence.
pub fn outer_fence_evidence(
    instance: &str,
    nics: &[String],
    generation: &str,
) -> Result<ProvisioningEvidence, DriverError> {
    if instance.is_empty() || !nics.is_empty() {
        return Err(invalid("LXD provisioning evidence is incomplete"));
    }
    let encoded = serde_json::to_vec(&LxdOuterFenceEvidence {
        instance,
        network_devices: nics,
    })
    .map_err(invalid)?;
    let evidence =
        ProvisioningEvidence::from_enforcement_evidence(generation, &encoded).map_err(invalid)?;
    evidence.validate(generation).map_err(invalid)?;
    Ok(evidence)
}

/// Builds the files for both instances. The workload receives only sandbox
/// credentials; supervisor credentials, gateway authorization and the restart
/// copy never enter the workload's filesystem.
pub fn bootstrap(input: BootstrapInput<'_>) -> Result<Bootstrap, DriverError> {
    let BootstrapInput {
        sandbox_id,
        instance,
        generation,
        host_gateway_ip,
        identity,
        child_env,
        launch_authentication,
    } = input;
    launch_authentication.validate().map_err(invalid)?;
    let session_id = launch_authentication.supervisor.session_id;
    let tls = generate_sandbox_tls_material(session_id).map_err(invalid)?;
    let runtime_generation = launch_authentication
        .supervisor
        .runtime_generation
        .to_string();
    let evidence = outer_fence_evidence(instance, &[], &runtime_generation)?;
    let resource_claims = BTreeMap::from([
        ("lxd.instance".to_owned(), instance.to_owned()),
        (
            "lxd.image_identity".to_owned(),
            identity.resource_digest.clone(),
        ),
    ]);
    let verification_keys = launch_authentication
        .verification_keys
        .iter()
        .map(|key| {
            String::from_utf8(key.public_key_pem.clone())
                .map(|public_key_pem| GatewayVerificationKey {
                    key_id: key.key_id.clone(),
                    public_key_pem,
                })
                .map_err(invalid)
        })
        .collect::<Result<Vec<_>, _>>()?;

    let config = BoundaryConfig {
        boundary_id: sandbox_id.into(),
        generation: runtime_generation.clone(),
        session_id,
        session_rotation: launch_authentication.supervisor.session_rotation,
        auth_epoch: launch_authentication.supervisor.auth_epoch,
        gateway_id: launch_authentication.gateway_id.clone(),
        verification_keys,
        listener: BoundaryListener::RuntimeInitiatedUnix {
            socket_path: CONTROL_SOCKET.into(),
            tls: SandboxTlsServerConfig {
                certificate_chain_path: PathBuf::from(format!("{SANDBOX_DIR}/server.crt")),
                private_key_path: PathBuf::from(format!("{SANDBOX_DIR}/server.key")),
            },
        },
        resource_claims: resource_claims.clone(),
        resource_claim_files: BTreeMap::new(),
        workload_identity: identity.clone(),
        provisioning_evidence: evidence.clone(),
        child_env: child_env.clone(),
    };
    let descriptor = SandboxRuntimeDescriptor {
        boundary_id: sandbox_id.into(),
        generation: runtime_generation,
        session_id,
        transport: SandboxTransport::RuntimeInitiatedUnix {
            socket_path: CONTROL_SOCKET.into(),
        },
        tls: SandboxTlsClientConfig {
            server_name: tls.server_name,
            trust_anchor_pem: tls.trust_anchor_pem,
        },
        host_gateway_ip: Some(host_gateway_ip),
        resource_claims,
        workload_identity: identity.clone(),
        provisioning_evidence: evidence,
    };
    let restart = RestartMetadata {
        generation: generation.to_owned(),
        workload_identity: identity.clone(),
        child_env,
    };

    let owned_dir = |mode| FileMeta {
        uid: identity.uid,
        gid: identity.gid,
        mode,
    };
    let secret = owned_dir(0o600);
    let workload = vec![
        Entry::dir("/.openshell", ROOT_DIR),
        Entry::dir(CHANNEL_ROOT, ROOT_DIR),
        Entry::dir(SANDBOX_DIR, owned_dir(0o711)),
        Entry::file(
            BOOTSTRAP_PATH,
            serde_json::to_vec(&config).map_err(invalid)?,
            secret,
        ),
        Entry::file(
            &format!("{SANDBOX_DIR}/server.crt"),
            tls.certificate_chain_pem.into_bytes(),
            secret,
        ),
        Entry::file(
            &format!("{SANDBOX_DIR}/server.key"),
            tls.private_key_pem.into_bytes(),
            secret,
        ),
    ];
    let supervisor = vec![
        Entry::dir("/.openshell", ROOT_DIR),
        Entry::dir("/.openshell/supervisor", owned_dir(0o700)),
        Entry::file(
            RUNTIME_DESCRIPTOR_PATH,
            serde_json::to_vec(&descriptor).map_err(invalid)?,
            secret,
        ),
        Entry::file(
            AUTH_BUNDLE_PATH,
            serde_json::to_vec(&launch_authentication.supervisor).map_err(invalid)?,
            secret,
        ),
        Entry::file(
            RESTART_METADATA_PATH,
            serde_json::to_vec(&restart).map_err(invalid)?,
            secret,
        ),
    ];
    Ok(Bootstrap {
        workload,
        supervisor,
    })
}

pub fn restart_metadata_from_slice(bytes: &[u8]) -> Result<RestartMetadata, DriverError> {
    serde_json::from_slice(bytes).map_err(invalid)
}

/// The control directory's owner is the workload identity, mode 0700: both
/// instances share one user namespace map, so the supervisor (same identity)
/// can bind the socket and the workload runtime can dial it.
pub fn control_dir_meta(identity: &ResolvedWorkloadIdentity) -> FileMeta {
    FileMeta {
        uid: identity.uid,
        gid: identity.gid,
        mode: 0o700,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fence_evidence_rejects_network_devices_and_blank_instances() {
        assert!(outer_fence_evidence("", &[], "g1").is_err());
        assert!(outer_fence_evidence("openshell-x", &["eth0".into()], "g1").is_err());
        let evidence = outer_fence_evidence("openshell-x", &[], "g1").unwrap();
        assert_eq!(evidence.generation, "g1");
        // The digest is stable for the same facts and differs per instance.
        assert_eq!(
            evidence.evidence_digest,
            outer_fence_evidence("openshell-x", &[], "g1")
                .unwrap()
                .evidence_digest
        );
        assert_ne!(
            evidence.evidence_digest,
            outer_fence_evidence("openshell-y", &[], "g1")
                .unwrap()
                .evidence_digest
        );
    }

    #[test]
    fn control_directory_belongs_to_the_workload_identity() {
        let identity =
            ResolvedWorkloadIdentity::new(1000, 1001, vec![], "image".into(), "sha256:x".into())
                .unwrap();
        let meta = control_dir_meta(&identity);
        assert_eq!((meta.uid, meta.gid, meta.mode), (1000, 1001, 0o700));
    }
}
