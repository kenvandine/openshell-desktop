//! View-model types for sandboxes and their policies.
//!
//! The data here is sample data until the app talks to the OpenShell gateway.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxState {
    Running,
    Stopped,
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
}

#[derive(Debug, Clone)]
pub struct Sandbox {
    pub name: String,
    pub runtime: String,
    pub profile: String,
    pub state: SandboxState,
    pub uptime_mins: u32,
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
        }
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
            runtime: "Python".into(),
            profile: "Code Assistant".into(),
            state: SandboxState::Running,
            uptime_mins: 24,
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
            }],
        },
        Sandbox {
            name: "data-scraper".into(),
            runtime: "Node".into(),
            profile: "Web Researcher".into(),
            state: SandboxState::Stopped,
            uptime_mins: 0,
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
            runtime: "Rust".into(),
            profile: "Fully Offline".into(),
            state: SandboxState::Running,
            uptime_mins: 132,
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
