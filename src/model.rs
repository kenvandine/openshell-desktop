//! View-model types for sandboxes and their policies.
//!
//! Sandboxes come from the OpenShell gateway, enriched with LXD details. While
//! the gateway is unreachable the app lists LXD instances directly.

use openshell_gateway_client::{PolicyView, Proposal, Snapshot, State};
use openshell_lxd::{Listed, parse_cpu_limit, parse_memory_mb};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxState {
    Running,
    Stopped,
    /// Provisioning, starting, stopping or failed; see `Sandbox::phase`.
    Busy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    ReadWrite,
    ReadOnly,
}

impl Access {
    pub fn short_label(self) -> &'static str {
        match self {
            Access::ReadWrite => "RW",
            Access::ReadOnly => "RO",
        }
    }
}

/// A host directory bind-mounted into the sandbox by an LXD disk device.
#[derive(Debug, Clone)]
pub struct Mount {
    /// The LXD device, which removing the mount deletes.
    pub device: String,
    pub host: String,
    pub sandbox: String,
    pub access: Access,
}

/// An allowed egress endpoint and the policy rule that allows it.
#[derive(Debug, Clone)]
pub struct NetworkRule {
    pub rule: String,
    pub host: String,
    pub port: u16,
}

/// An outbound request blocked by the OpenShell proxy, awaiting a decision.
#[derive(Debug, Clone)]
pub struct AdvisorPrompt {
    pub method: String,
    pub url: String,
    pub host: String,
    pub port: u16,
    pub binary: String,
    /// Gateway draft chunk and the token to approve it with.
    pub chunk_id: String,
    pub review_token: String,
}

impl AdvisorPrompt {
    pub fn to_proposal(&self) -> Proposal {
        Proposal {
            chunk_id: self.chunk_id.clone(),
            review_token: self.review_token.clone(),
            host: self.host.clone(),
            port: self.port,
            binary: self.binary.clone(),
            rationale: String::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Sandbox {
    /// Display name.
    pub name: String,
    /// LXD instance name, which the console execs into.
    pub instance: String,
    pub runtime: String,
    pub profile: String,
    pub state: SandboxState,
    /// The gateway's phase for this sandbox, empty for LXD-only data.
    pub phase: String,
    /// Why the gateway says it is not ready, if it says.
    pub status_detail: String,
    /// "Container" or "Virtual Machine".
    pub instance_type: String,
    pub uptime_mins: Option<u32>,
    pub memory_used_mb: u32,
    pub memory_limit_mb: u32,
    pub cpu_limit: u32,
    pub gpu_passthrough: bool,
    pub mounts: Vec<Mount>,
    pub rules: Vec<NetworkRule>,
    pub prompts: Vec<AdvisorPrompt>,
    /// The gateway's policy; `None` for LXD-only data or while provisioning.
    pub policy: Option<PolicyView>,
    /// Catalog id of the agent the app installs and runs in it.
    pub agent: Option<String>,
}

impl Sandbox {
    pub fn summary(&self) -> String {
        match self.state {
            SandboxState::Running => {
                format!("{} · {}", self.runtime, format_mb(self.memory_used_mb))
            }
            SandboxState::Stopped => format!("{} · Stopped", self.runtime),
            SandboxState::Busy => format!("{} · {}", self.runtime, self.phase),
        }
    }
}

impl Sandbox {
    /// A sandbox view of a listed LXD instance. Policy fields stay empty until
    /// the OpenShell gateway provides them.
    pub fn from_listed(listed: &Listed) -> Self {
        let instance = listed.instance;
        let config = |key: &str| instance.expanded_config.get(key).map(String::as_str);
        // Host folders only: the root disk has no source, and the driver's
        // shared volume is a storage volume, not a host path.
        let mut mounts: Vec<Mount> = instance
            .expanded_devices
            .iter()
            .filter(|(_, d)| d.get("type").map(String::as_str) == Some("disk"))
            .filter(|(_, d)| d.get("source").is_some_and(|s| s.starts_with('/')))
            .filter_map(|(device, d)| {
                Some(Mount {
                    device: device.clone(),
                    host: d.get("source")?.clone(),
                    sandbox: d.get("path")?.clone(),
                    access: if d.get("readonly").map(String::as_str) == Some("true") {
                        Access::ReadOnly
                    } else {
                        Access::ReadWrite
                    },
                })
            })
            .collect();
        mounts.sort_by(|a, b| a.sandbox.cmp(&b.sandbox));

        Sandbox {
            name: instance
                .openshell_sandbox_name()
                .unwrap_or(&instance.name)
                .to_owned(),
            instance: instance.name.clone(),
            runtime: if instance.kind == "virtual-machine" {
                "VM"
            } else {
                "Container"
            }
            .into(),
            profile: instance.profiles.join(", "),
            state: if listed.running {
                SandboxState::Running
            } else {
                SandboxState::Stopped
            },
            phase: String::new(),
            status_detail: String::new(),
            instance_type: if instance.kind == "virtual-machine" {
                "Virtual Machine"
            } else {
                "Container"
            }
            .into(),
            uptime_mins: None,
            memory_used_mb: instance
                .state
                .as_ref()
                .and_then(|s| s.memory.as_ref())
                .map_or(0, |m| (m.usage / (1024 * 1024)) as u32),
            memory_limit_mb: config("limits.memory")
                .and_then(parse_memory_mb)
                .unwrap_or(0),
            cpu_limit: parse_cpu_limit(&instance.expanded_config).unwrap_or(0),
            gpu_passthrough: instance
                .expanded_devices
                .values()
                .any(|d| d.get("type").map(String::as_str) == Some("gpu")),
            mounts,
            rules: vec![],
            prompts: vec![],
            policy: None,
            agent: None,
        }
    }
}

impl Sandbox {
    /// A sandbox as the gateway reports it, enriched with LXD details when the
    /// matching instance is visible.
    pub fn from_gateway(snapshot: &Snapshot, lxd: Option<&Sandbox>) -> Self {
        let info = &snapshot.info;
        let mut sandbox = lxd.cloned().unwrap_or_else(|| Sandbox {
            name: info.name.clone(),
            instance: String::new(),
            runtime: "Container".into(),
            profile: info.image.clone(),
            state: SandboxState::Stopped,
            phase: String::new(),
            status_detail: String::new(),
            instance_type: "Container".into(),
            uptime_mins: None,
            memory_used_mb: 0,
            memory_limit_mb: 0,
            cpu_limit: 0,
            gpu_passthrough: false,
            mounts: vec![],
            rules: vec![],
            prompts: vec![],
            policy: None,
            agent: None,
        });
        sandbox.name = info.name.clone();
        sandbox.state = match info.state {
            State::Running => SandboxState::Running,
            State::Stopped => SandboxState::Stopped,
            State::Transitioning | State::Failed => SandboxState::Busy,
        };
        sandbox.phase = info.phase.clone();
        sandbox.agent = info.agent.clone();
        sandbox.status_detail = info.detail.clone();
        sandbox.rules = snapshot
            .policy
            .iter()
            .flat_map(PolicyView::endpoints)
            .map(|(rule, e)| NetworkRule {
                rule: rule.to_owned(),
                host: e.host.clone(),
                port: e.port,
            })
            .collect();
        sandbox.policy = snapshot.policy.clone();
        sandbox.prompts = snapshot
            .proposals
            .iter()
            .map(|p| AdvisorPrompt {
                method: String::new(),
                url: format!("{}:{}", p.host, p.port),
                host: p.host.clone(),
                port: p.port,
                binary: p.binary.clone(),
                chunk_id: p.chunk_id.clone(),
                review_token: p.review_token.clone(),
            })
            .collect();
        sandbox
    }
}

pub fn format_mb(mb: u32) -> String {
    if mb >= 1024 {
        format!("{:.1} GB", mb as f64 / 1024.0)
    } else {
        format!("{mb} MB")
    }
}
