//! Client for the OpenShell gateway's user-facing gRPC API.
//!
//! The desktop app talks to the gateway bundled in the snap over mTLS, using
//! the client certificate `openshell-gateway generate-certs` wrote next to the
//! gateway's state. Everything here is async; the app runs it on its own
//! runtime thread.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use openshell_core::proto::datamodel::v1::{WorkspaceSelector, workspace_selector::Selection};
use openshell_core::proto::{
    AddNetworkRule, ApproveDraftChunkRequest, CreateSandboxRequest, DeleteSandboxRequest,
    GetDraftPolicyRequest, GetSandboxConfigRequest, GpuResourceRequirements, ListSandboxesRequest,
    NetworkBinary, NetworkEndpoint, NetworkPolicyRule, PolicyChunk, PolicyMergeOperation,
    RejectDraftChunkRequest, RemoveNetworkEndpoint, RemoveNetworkRule, ResourceRequirements,
    Sandbox, SandboxPhase, SandboxPolicy, SandboxSpec, SettingScope, SettingValue,
    StartSandboxRequest, StopSandboxRequest, UpdateConfigRequest,
    open_shell_client::OpenShellClient, policy_merge_operation::Operation, setting_value,
};
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Endpoint, Identity};

mod exec;
mod providers;

pub use exec::{ExecInput, ExecOptions, ExecOutput, ExecOutputStream};
pub use openshell_core::proto;
pub use providers::{CredentialField, ProfileInfo, ProviderInfo, bundled_profiles};
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
    /// Why the sandbox is not ready, from its unmet conditions; empty when
    /// it is.
    pub detail: String,
    pub image: String,
    /// The agent the app installed for it (`AGENT_LABEL`), if any.
    pub agent: Option<String>,
}

/// Label naming the agent a sandbox was created for, by catalog id.
pub const AGENT_LABEL: &str = "openshell-desktop/agent";

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
            detail: sandbox
                .status
                .as_ref()
                .map(|status| {
                    status
                        .conditions
                        .iter()
                        .filter(|c| c.status != "True" && !c.message.is_empty())
                        .map(|c| c.message.clone())
                        .collect::<Vec<_>>()
                        .join("; ")
                })
                .unwrap_or_default(),
            image: sandbox
                .spec
                .as_ref()
                .and_then(|s| s.template.as_ref())
                .map(|t| t.image.clone())
                .unwrap_or_default(),
            agent: meta.and_then(|m| m.labels.get(AGENT_LABEL)).cloned(),
        }
    }
}

/// An allowed outbound endpoint from a sandbox's network policy.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct AllowedEndpoint {
    pub host: String,
    pub port: u16,
}

/// A named network rule: where it allows traffic to, and for which binaries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyRule {
    pub name: String,
    pub endpoints: Vec<AllowedEndpoint>,
    /// Executables allowed to use the rule; empty means any binary.
    pub binaries: Vec<String>,
}

/// The parts of a sandbox policy the app shows. Only the network rules can
/// change after creation; the gateway rejects edits to everything else.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PolicyView {
    pub version: u32,
    /// Whether the workspace directory is writable.
    pub include_workdir: bool,
    pub read_only: Vec<String>,
    pub read_write: Vec<String>,
    pub run_as_user: String,
    pub run_as_group: String,
    /// Landlock compatibility mode, such as `best_effort`.
    pub landlock: String,
    /// Sorted by name.
    pub rules: Vec<PolicyRule>,
}

impl PolicyView {
    pub fn from_proto(policy: &SandboxPolicy, version: u32) -> Self {
        let filesystem = policy.filesystem.clone().unwrap_or_default();
        let process = policy.process.clone().unwrap_or_default();
        let mut rules: Vec<_> = policy
            .network_policies
            .iter()
            .map(|(key, rule)| PolicyRule {
                name: key.clone(),
                endpoints: endpoints_of(rule),
                binaries: rule.binaries.iter().map(|b| b.path.clone()).collect(),
            })
            .collect();
        rules.sort_by(|a, b| a.name.cmp(&b.name));
        Self {
            version,
            include_workdir: filesystem.include_workdir,
            read_only: filesystem.read_only,
            read_write: filesystem.read_write,
            run_as_user: process.run_as_user,
            run_as_group: process.run_as_group,
            landlock: policy
                .landlock
                .as_ref()
                .map(|l| l.compatibility.clone())
                .unwrap_or_default(),
            rules,
        }
    }

    /// Every allowed endpoint with the rule that allows it, in display order.
    pub fn endpoints(&self) -> impl Iterator<Item = (&str, &AllowedEndpoint)> {
        self.rules
            .iter()
            .flat_map(|rule| rule.endpoints.iter().map(|e| (rule.name.as_str(), e)))
    }
}

/// One entry per port: an endpoint may list several in `ports`.
fn endpoints_of(rule: &NetworkPolicyRule) -> Vec<AllowedEndpoint> {
    let mut endpoints: Vec<_> = rule
        .endpoints
        .iter()
        .flat_map(|e| {
            let ports = if e.ports.is_empty() {
                vec![e.port]
            } else {
                e.ports.clone()
            };
            ports.into_iter().map(|port| AllowedEndpoint {
                host: e.host.clone(),
                port: u16::try_from(port).unwrap_or(0),
            })
        })
        .collect();
    endpoints.sort();
    endpoints.dedup();
    endpoints
}

/// The name the app gives a rule it adds for `host:port`, such as
/// `desktop_api_github_com_443`. Adding the same endpoint twice reuses it.
pub fn rule_name_for(host: &str, port: u16) -> String {
    let mut name = String::from("desktop_");
    for c in host.chars() {
        if c.is_ascii_alphanumeric() {
            name.push(c.to_ascii_lowercase());
        } else if !name.ends_with('_') {
            name.push('_');
        }
    }
    if !name.ends_with('_') {
        name.push('_');
    }
    name.push_str(&port.to_string());
    name
}

/// What a new sandbox asks for beyond its name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CreateOptions {
    /// Blank leaves the choice to the gateway's default.
    pub image: String,
    /// CPU cores; 0 means no limit.
    pub cpus: u32,
    /// Memory in MiB; 0 means no limit.
    pub memory_mb: u32,
    pub gpu: bool,
    /// Providers whose credentials the sandbox may use.
    pub providers: Vec<String>,
    /// The sandbox's main process; empty leaves the gateway's login shell.
    pub command: Vec<String>,
    pub environment: Vec<(String, String)>,
    pub labels: Vec<(String, String)>,
}

impl CreateOptions {
    /// The spec the gateway expects: limits as Kubernetes-style quantities
    /// under `template.resources.limits`, and a GPU as a resource requirement.
    pub fn spec(&self) -> SandboxSpec {
        use prost_types::{Struct, Value, value::Kind};
        let string = |s: String| Value {
            kind: Some(Kind::StringValue(s)),
        };
        let mut limits = std::collections::BTreeMap::new();
        if self.cpus > 0 {
            limits.insert("cpu".to_owned(), string(self.cpus.to_string()));
        }
        if self.memory_mb > 0 {
            limits.insert("memory".to_owned(), string(format!("{}Mi", self.memory_mb)));
        }
        let resources = (!limits.is_empty()).then(|| Struct {
            fields: [(
                "limits".to_owned(),
                Value {
                    kind: Some(Kind::StructValue(Struct { fields: limits })),
                },
            )]
            .into(),
        });
        let template = (!self.image.is_empty() || resources.is_some()).then(|| {
            openshell_core::proto::SandboxTemplate {
                image: self.image.clone(),
                resources,
                ..Default::default()
            }
        });
        SandboxSpec {
            template,
            providers: self.providers.clone(),
            command: self.command.clone(),
            tty: !self.command.is_empty(),
            environment: self.environment.iter().cloned().collect(),
            resource_requirements: self.gpu.then_some(ResourceRequirements {
                gpu: Some(GpuResourceRequirements { count: Some(1) }),
            }),
            ..Default::default()
        }
    }
}

/// A blocked request the policy advisor proposes to allow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proposal {
    pub chunk_id: String,
    /// Must be echoed on approval so the gateway knows what was reviewed.
    pub review_token: String,
    pub host: String,
    pub port: u16,
    pub binary: String,
    pub rationale: String,
}

impl Proposal {
    /// The first endpoint of a pending chunk; chunks without one have nothing
    /// to show or approve.
    pub fn from_chunk(chunk: &PolicyChunk) -> Option<Self> {
        let rule = chunk.proposed_rule.as_ref()?;
        let endpoint = rule.endpoints.first()?;
        let binary = if chunk.binary.is_empty() {
            rule.binaries
                .first()
                .map(|b| b.path.clone())
                .unwrap_or_default()
        } else {
            chunk.binary.clone()
        };
        Some(Self {
            chunk_id: chunk.id.clone(),
            review_token: chunk.review_token.clone(),
            host: endpoint.host.clone(),
            port: u16::try_from(endpoint.port).unwrap_or(0),
            binary,
            rationale: chunk.rationale.clone(),
        })
    }
}

/// The gateway setting that lets it approve proposals on its own.
const APPROVAL_MODE_KEY: &str = "proposal_approval_mode";

/// Whether the gateway approves a sandbox's proposals by itself. It only does
/// so when its prover finds a proposal grants nothing new beyond the request;
/// anything riskier still waits for the user.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AutoApprove {
    pub enabled: bool,
    /// Set gateway-wide, which overrides any per-sandbox choice.
    pub gateway_wide: bool,
}

impl AutoApprove {
    fn from_settings(settings: &HashMap<String, proto::EffectiveSetting>) -> Self {
        let Some(setting) = settings.get(APPROVAL_MODE_KEY) else {
            return Self::default();
        };
        let enabled = matches!(
            setting.value.as_ref().and_then(|v| v.value.as_ref()),
            Some(setting_value::Value::StringValue(mode)) if mode == "auto"
        );
        Self {
            enabled,
            gateway_wide: setting.scope == SettingScope::Global as i32,
        }
    }
}

/// A sandbox with its policy and pending advisor proposals.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub info: SandboxInfo,
    /// `None` while the gateway has no policy for it, such as during provisioning.
    pub policy: Option<PolicyView>,
    pub proposals: Vec<Proposal>,
    pub auto_approve: AutoApprove,
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

    /// Whether the gateway's compute driver can give a sandbox a GPU.
    pub async fn gpu_supported(&mut self) -> Result<bool, Error> {
        let info = self
            .client
            .get_gateway_info(openshell_core::proto::GetGatewayInfoRequest {})
            .await?
            .into_inner();
        Ok(info.compute_drivers.iter().any(|driver| {
            driver
                .capabilities
                .as_ref()
                .and_then(|c| c.resource_capabilities.as_ref())
                .and_then(|r| r.gpu.as_ref())
                .is_some_and(|gpu| gpu.default_selection_supported)
        }))
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

    /// Every sandbox with its policy and pending proposals. Policy lookups
    /// that fail (a sandbox still provisioning has none) leave those parts empty.
    pub async fn snapshot(&mut self) -> Result<Vec<Snapshot>, Error> {
        let infos = self.list().await?;
        let mut snapshots = Vec::with_capacity(infos.len());
        for info in infos {
            let config = self
                .client
                .get_sandbox_config(GetSandboxConfigRequest {
                    workspace_scope: workspace(),
                    name: info.name.clone(),
                })
                .await
                .ok()
                .map(|r| r.into_inner());
            let policy = config
                .as_ref()
                .and_then(|r| Some(PolicyView::from_proto(r.policy.as_ref()?, r.version)));
            let auto_approve = config
                .as_ref()
                .map(|r| AutoApprove::from_settings(&r.settings))
                .unwrap_or_default();
            let proposals = self
                .client
                .get_draft_policy(GetDraftPolicyRequest {
                    workspace_scope: workspace(),
                    status_filter: "pending".into(),
                    sandbox: info.name.clone(),
                })
                .await
                .ok()
                .map(|r| r.into_inner().chunks)
                .unwrap_or_default()
                .iter()
                .filter_map(Proposal::from_chunk)
                .collect();
            snapshots.push(Snapshot {
                info,
                policy,
                proposals,
                auto_approve,
            });
        }
        Ok(snapshots)
    }

    pub async fn approve(&mut self, sandbox: &str, proposal: &Proposal) -> Result<(), Error> {
        self.client
            .approve_draft_chunk(ApproveDraftChunkRequest {
                workspace_scope: workspace(),
                chunk_id: proposal.chunk_id.clone(),
                review_token: proposal.review_token.clone(),
                sandbox: sandbox.into(),
                ..Default::default()
            })
            .await?;
        Ok(())
    }

    pub async fn reject(&mut self, sandbox: &str, chunk_id: &str) -> Result<(), Error> {
        self.client
            .reject_draft_chunk(RejectDraftChunkRequest {
                workspace_scope: workspace(),
                chunk_id: chunk_id.into(),
                sandbox: sandbox.into(),
                ..Default::default()
            })
            .await?;
        Ok(())
    }

    /// Lets the gateway approve `sandbox`'s low-risk proposals by itself.
    /// The gateway cannot delete a sandbox setting, so off is "manual".
    pub async fn set_auto_approve(&mut self, sandbox: &str, enabled: bool) -> Result<(), Error> {
        let mode = if enabled { "auto" } else { "manual" };
        self.client
            .update_config(UpdateConfigRequest {
                workspace_scope: workspace(),
                sandbox: sandbox.into(),
                setting_key: APPROVAL_MODE_KEY.into(),
                setting_value: Some(SettingValue {
                    value: Some(setting_value::Value::StringValue(mode.into())),
                }),
                ..Default::default()
            })
            .await?;
        Ok(())
    }

    /// Allows `host:port` under a rule of its own, optionally only for one
    /// executable. Returns the new policy version.
    pub async fn add_rule(
        &mut self,
        sandbox: &str,
        host: &str,
        port: u16,
        binary: Option<&str>,
    ) -> Result<u32, Error> {
        let rule_name = rule_name_for(host, port);
        self.merge(
            sandbox,
            Operation::AddRule(AddNetworkRule {
                rule_name: rule_name.clone(),
                rule: Some(NetworkPolicyRule {
                    name: rule_name,
                    endpoints: vec![NetworkEndpoint {
                        host: host.into(),
                        port: port.into(),
                        ..Default::default()
                    }],
                    binaries: binary
                        .map(|path| NetworkBinary { path: path.into() })
                        .into_iter()
                        .collect(),
                }),
            }),
        )
        .await
    }

    /// Removes one endpoint from a rule; the gateway drops a rule left empty.
    pub async fn remove_endpoint(
        &mut self,
        sandbox: &str,
        rule: &str,
        host: &str,
        port: u16,
    ) -> Result<u32, Error> {
        self.merge(
            sandbox,
            Operation::RemoveEndpoint(RemoveNetworkEndpoint {
                rule_name: rule.into(),
                host: host.into(),
                port: port.into(),
            }),
        )
        .await
    }

    pub async fn remove_rule(&mut self, sandbox: &str, rule: &str) -> Result<u32, Error> {
        self.merge(
            sandbox,
            Operation::RemoveRule(RemoveNetworkRule {
                rule_name: rule.into(),
            }),
        )
        .await
    }

    /// Applies one incremental change to a sandbox's network policy.
    async fn merge(&mut self, sandbox: &str, operation: Operation) -> Result<u32, Error> {
        let response = self
            .client
            .update_config(UpdateConfigRequest {
                workspace_scope: workspace(),
                sandbox: sandbox.into(),
                merge_operations: vec![PolicyMergeOperation {
                    operation: Some(operation),
                }],
                ..Default::default()
            })
            .await?
            .into_inner();
        Ok(response.version)
    }

    pub async fn create(
        &mut self,
        name: &str,
        options: &CreateOptions,
    ) -> Result<SandboxInfo, Error> {
        let spec = options.spec();
        let response = self
            .client
            .create_sandbox(CreateSandboxRequest {
                workspace_scope: workspace(),
                name: name.into(),
                spec: Some(spec),
                labels: options.labels.iter().cloned().collect(),
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
                detail: String::new(),
                image: options.image.clone(),
                agent: options
                    .labels
                    .iter()
                    .find(|(key, _)| key == AGENT_LABEL)
                    .map(|(_, value)| value.clone()),
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
    fn auto_approve_reads_the_effective_setting() {
        let setting = |mode: &str, scope: SettingScope| {
            HashMap::from([(
                APPROVAL_MODE_KEY.to_owned(),
                proto::EffectiveSetting {
                    value: Some(SettingValue {
                        value: Some(setting_value::Value::StringValue(mode.into())),
                    }),
                    scope: scope as i32,
                },
            )])
        };
        assert_eq!(
            AutoApprove::from_settings(&HashMap::new()),
            AutoApprove::default()
        );
        assert_eq!(
            AutoApprove::from_settings(&setting("auto", SettingScope::Sandbox)),
            AutoApprove {
                enabled: true,
                gateway_wide: false
            }
        );
        assert_eq!(
            AutoApprove::from_settings(&setting("manual", SettingScope::Global)),
            AutoApprove {
                enabled: false,
                gateway_wide: true
            }
        );
        // Registered but unset: an empty value is manual.
        let unset = HashMap::from([(
            APPROVAL_MODE_KEY.to_owned(),
            proto::EffectiveSetting::default(),
        )]);
        assert!(!AutoApprove::from_settings(&unset).enabled);
    }

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
    fn proposals_come_from_the_first_endpoint_of_a_pending_chunk() {
        use openshell_core::proto::{NetworkBinary, NetworkEndpoint, NetworkPolicyRule};
        let chunk = PolicyChunk {
            id: "c1".into(),
            review_token: "tok".into(),
            proposed_rule: Some(NetworkPolicyRule {
                endpoints: vec![NetworkEndpoint {
                    host: "registry.npmjs.org".into(),
                    port: 443,
                    ..Default::default()
                }],
                binaries: vec![NetworkBinary {
                    path: "/usr/bin/npm".into(),
                }],
                ..Default::default()
            }),
            ..Default::default()
        };
        let proposal = Proposal::from_chunk(&chunk).unwrap();
        assert_eq!(
            (
                proposal.host.as_str(),
                proposal.port,
                proposal.binary.as_str()
            ),
            ("registry.npmjs.org", 443, "/usr/bin/npm")
        );
        assert!(Proposal::from_chunk(&PolicyChunk::default()).is_none());
    }

    #[test]
    fn policy_view_sorts_rules_and_expands_ports() {
        use openshell_core::proto::{FilesystemPolicy, NetworkEndpoint, NetworkPolicyRule};
        let rule = |hosts: &[&str]| NetworkPolicyRule {
            endpoints: hosts
                .iter()
                .map(|h| NetworkEndpoint {
                    host: (*h).into(),
                    port: 443,
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        };
        let mut multi = rule(&["b.example"]);
        multi.endpoints[0].ports = vec![80, 443];
        let policy = SandboxPolicy {
            filesystem: Some(FilesystemPolicy {
                include_workdir: true,
                read_only: vec!["/usr".into()],
                read_write: vec![],
            }),
            network_policies: [
                (
                    "z".to_owned(),
                    rule(&["b.example", "a.example", "a.example"]),
                ),
                ("a".to_owned(), multi),
            ]
            .into(),
            ..Default::default()
        };
        let view = PolicyView::from_proto(&policy, 3);
        assert_eq!(view.version, 3);
        assert!(view.include_workdir);
        let flat: Vec<_> = view
            .endpoints()
            .map(|(rule, e)| format!("{rule} {}:{}", e.host, e.port))
            .collect();
        assert_eq!(
            flat,
            [
                "a b.example:80",
                "a b.example:443",
                "z a.example:443",
                "z b.example:443"
            ]
        );
    }

    #[test]
    fn rule_names_are_stable_identifiers() {
        assert_eq!(
            rule_name_for("api.github.com", 443),
            "desktop_api_github_com_443"
        );
        assert_eq!(rule_name_for("*.Example.org", 80), "desktop_example_org_80");
    }

    #[test]
    fn create_options_become_limits_and_a_gpu_request() {
        let spec = CreateOptions {
            image: String::new(),
            cpus: 2,
            memory_mb: 4096,
            gpu: true,
            ..Default::default()
        }
        .spec();
        let template = spec.template.unwrap();
        assert!(template.image.is_empty());
        let limits = match &template.resources.unwrap().fields["limits"].kind {
            Some(prost_types::value::Kind::StructValue(s)) => s.clone(),
            other => panic!("limits is {other:?}"),
        };
        let quantity = |key: &str| match &limits.fields[key].kind {
            Some(prost_types::value::Kind::StringValue(v)) => v.clone(),
            other => panic!("{key} is {other:?}"),
        };
        assert_eq!(
            (quantity("cpu"), quantity("memory")),
            ("2".into(), "4096Mi".into())
        );
        assert_eq!(
            spec.resource_requirements.unwrap().gpu.unwrap().count,
            Some(1)
        );

        let bare = CreateOptions::default().spec();
        assert!(bare.template.is_none() && bare.resource_requirements.is_none());
        assert!(bare.command.is_empty() && !bare.tty);
    }

    #[test]
    fn create_options_carry_providers_and_the_agent_command() {
        let spec = CreateOptions {
            providers: vec!["claude".into()],
            command: vec!["claude".into()],
            environment: vec![("TERM".into(), "xterm-256color".into())],
            ..Default::default()
        }
        .spec();
        assert_eq!(spec.providers, ["claude"]);
        assert_eq!(spec.command, ["claude"]);
        assert!(spec.tty, "an agent's main process needs a terminal");
        assert_eq!(spec.environment["TERM"], "xterm-256color");
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
