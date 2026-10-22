//! View-model types for sandboxes and their policies.
//!
//! Sandboxes come from live LXD instances when LXD is reachable, otherwise
//! from sample data, until the app talks to the OpenShell gateway.

use openshell_gateway_client::{Proposal, Snapshot, State};
use openshell_lxd::{Listed, parse_memory_mb};

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
    Hidden,
}

impl Access {
    pub fn short_label(self) -> &'static str {
        match self {
            Access::ReadWrite => "RW",
            Access::ReadOnly => "RO",
            Access::Hidden => "Hidden",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Mount {
    pub host: String,
    pub sandbox: String,
    pub access: Access,
}

#[derive(Debug, Clone)]
pub struct NetworkRule {
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
    /// Gateway draft chunk and the token to approve it with; empty for samples.
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
    /// The gateway's phase for this sandbox, empty for sample and LXD-only data.
    pub phase: String,
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
        let mounts = instance
            .expanded_devices
            .values()
            .filter(|d| d.get("type").map(String::as_str) == Some("disk"))
            .filter_map(|d| {
                Some(Mount {
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
            cpu_limit: config("limits.cpu")
                .and_then(|c| c.parse().ok())
                .unwrap_or(0),
            gpu_passthrough: instance
                .expanded_devices
                .values()
                .any(|d| d.get("type").map(String::as_str) == Some("gpu")),
            mounts,
            rules: vec![],
            prompts: vec![],
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
            instance_type: "Container".into(),
            uptime_mins: None,
            memory_used_mb: 0,
            memory_limit_mb: 0,
            cpu_limit: 0,
            gpu_passthrough: false,
            mounts: vec![],
            rules: vec![],
            prompts: vec![],
        });
        sandbox.name = info.name.clone();
        sandbox.state = match info.state {
            State::Running => SandboxState::Running,
            State::Stopped => SandboxState::Stopped,
            State::Transitioning | State::Failed => SandboxState::Busy,
        };
        sandbox.phase = info.phase.clone();
        sandbox.rules = snapshot
            .allowed
            .iter()
            .map(|e| NetworkRule {
                host: e.host.clone(),
                port: e.port,
            })
            .collect();
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

fn system_mounts() -> Mount {
    Mount {
        host: "/etc, /usr, /lib".into(),
        sandbox: "System".into(),
        access: Access::ReadOnly,
    }
}

fn rule(host: &str, port: u16) -> NetworkRule {
    NetworkRule {
        host: host.into(),
        port,
    }
}

pub fn sample_sandboxes() -> Vec<Sandbox> {
    vec![
        Sandbox {
            name: "code-eval-01".into(),
            instance: "code-eval-01".into(),
            runtime: "Python".into(),
            profile: "Code Assistant".into(),
            state: SandboxState::Running,
            phase: String::new(),
            instance_type: "Container".into(),
            uptime_mins: Some(24),
            memory_used_mb: 840,
            memory_limit_mb: 4096,
            cpu_limit: 4,
            gpu_passthrough: false,
            mounts: vec![
                Mount {
                    host: "~/Projects/web-app".into(),
                    sandbox: "/workspace".into(),
                    access: Access::ReadWrite,
                },
                system_mounts(),
            ],
            rules: vec![rule("api.openai.com", 443), rule("huggingface.co", 443)],
            prompts: vec![AdvisorPrompt {
                method: "GET".into(),
                url: "https://registry.npmjs.org".into(),
                host: "registry.npmjs.org".into(),
                port: 443,
                binary: "/usr/bin/npm".into(),
                chunk_id: String::new(),
                review_token: String::new(),
            }],
        },
        Sandbox {
            name: "data-scraper".into(),
            instance: "data-scraper".into(),
            runtime: "Node".into(),
            profile: "Web Researcher".into(),
            state: SandboxState::Stopped,
            phase: String::new(),
            instance_type: "Container".into(),
            uptime_mins: None,
            memory_used_mb: 0,
            memory_limit_mb: 2048,
            cpu_limit: 2,
            gpu_passthrough: false,
            mounts: vec![system_mounts()],
            rules: vec![rule("*", 443), rule("*", 80)],
            prompts: vec![],
        },
        Sandbox {
            name: "doc-indexer".into(),
            instance: "doc-indexer".into(),
            runtime: "Rust".into(),
            profile: "Fully Offline".into(),
            state: SandboxState::Running,
            phase: String::new(),
            instance_type: "Container".into(),
            uptime_mins: Some(132),
            memory_used_mb: 512,
            memory_limit_mb: 8192,
            cpu_limit: 8,
            gpu_passthrough: true,
            mounts: vec![
                Mount {
                    host: "~/Documents/corpus".into(),
                    sandbox: "/data".into(),
                    access: Access::ReadOnly,
                },
                Mount {
                    host: "~/.ssh".into(),
                    sandbox: "/root/.ssh".into(),
                    access: Access::Hidden,
                },
                system_mounts(),
            ],
            rules: vec![],
            prompts: vec![],
        },
    ]
}
