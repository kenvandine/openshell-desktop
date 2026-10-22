//! Background connection to the OpenShell gateway.
//!
//! The gateway client is async, the UI is not. A dedicated thread runs a tokio
//! runtime, polls the sandbox list and executes commands; the window sends
//! `Command`s in and receives `Event`s on the GTK main loop.

use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

use openshell_gateway_client::codex::CodexTokens;
use openshell_gateway_client::{
    Config, CreateOptions, Error, Gateway, ProfileInfo, Proposal, ProviderInfo, Snapshot,
};

const POLL: Duration = Duration::from_secs(3);

/// Credential values by env var name. Debug output names only the keys.
#[derive(Clone)]
pub struct Credentials(pub BTreeMap<String, String>);

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_set().entries(self.0.keys()).finish()
    }
}

#[derive(Debug, Clone)]
pub enum Command {
    Refresh,
    Create {
        name: String,
        options: CreateOptions,
    },
    Start(String),
    Stop(String),
    Delete(String),
    /// Allow the endpoint a blocked request needs.
    Approve {
        sandbox: String,
        proposal: Proposal,
    },
    Reject {
        sandbox: String,
        chunk_id: String,
    },
    /// Allow `host:port`, for one executable or (with `None`) any.
    AddRule {
        sandbox: String,
        host: String,
        port: u16,
        binary: Option<String>,
    },
    RemoveEndpoint {
        sandbox: String,
        rule: String,
        host: String,
        port: u16,
    },
    RemoveRule {
        sandbox: String,
        rule: String,
    },
    /// Let the gateway approve low-risk requests itself, or ask every time.
    SetAutoApprove {
        sandbox: String,
        enabled: bool,
    },
    /// Store credentials as provider `name` of profile `kind`.
    CreateProvider {
        name: String,
        kind: String,
        credentials: Credentials,
    },
    /// Store a Codex sign-in as provider `name`, renewed by the gateway.
    CreateCodexProvider {
        name: String,
        tokens: CodexTokens,
    },
    DeleteProvider(String),
}

impl Command {
    /// What the command does, for "… failed" messages; `None` for polling.
    fn describe(&self) -> Option<String> {
        Some(match self {
            Command::Refresh => return None,
            Command::Create { name, .. } => format!("Creating {name}"),
            Command::Start(name) => format!("Starting {name}"),
            Command::Stop(name) => format!("Stopping {name}"),
            Command::Delete(name) => format!("Deleting {name}"),
            Command::Approve { proposal, .. } => {
                format!("Allowing {}:{}", proposal.host, proposal.port)
            }
            Command::Reject { .. } => "Ignoring the request".to_owned(),
            Command::AddRule { host, port, .. } => format!("Allowing {host}:{port}"),
            Command::RemoveEndpoint { host, port, .. } => format!("Removing {host}:{port}"),
            Command::RemoveRule { rule, .. } => format!("Removing rule {rule}"),
            Command::SetAutoApprove { sandbox, .. } => {
                format!("Changing how {sandbox} asks for approval")
            }
            Command::CreateProvider { name, .. } => format!("Adding account {name}"),
            Command::CreateCodexProvider { name, .. } => {
                format!("Adding Codex account {name}")
            }
            Command::DeleteProvider(name) => format!("Removing account {name}"),
        })
    }
}

#[derive(Debug, Clone)]
pub enum Event {
    /// Current sandboxes; the gateway is reachable.
    Sandboxes(Vec<Snapshot>),
    /// The gateway cannot be reached; the reason is shown in the status tooltip.
    Unavailable(String),
    /// A command succeeded.
    Done(String),
    /// A command failed.
    Failed(String),
    /// Whether new sandboxes can ask for a GPU; sent on each connect.
    GpuSupported(bool),
    /// Provider types and stored providers; sent on connect and whenever an
    /// account is added or removed.
    Accounts {
        profiles: Vec<ProfileInfo>,
        providers: Vec<ProviderInfo>,
    },
}

pub struct Handle {
    pub commands: async_channel::Sender<Command>,
    pub events: async_channel::Receiver<Event>,
}

pub fn spawn() -> Handle {
    let (command_tx, command_rx) = async_channel::unbounded();
    let (event_tx, event_rx) = async_channel::unbounded();
    std::thread::Builder::new()
        .name("gateway".into())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("tokio runtime");
            runtime.block_on(run(command_rx, event_tx));
        })
        .expect("spawn gateway thread");
    Handle {
        commands: command_tx,
        events: event_rx,
    }
}

async fn run(commands: async_channel::Receiver<Command>, events: async_channel::Sender<Event>) {
    let config = Config::discover();
    let mut gateway: Option<Gateway> = None;
    let mut tick = tokio::time::interval(POLL);
    loop {
        let command = tokio::select! {
            _ = tick.tick() => Command::Refresh,
            received = commands.recv() => match received {
                Ok(command) => command,
                Err(_) => return,
            },
        };

        if gateway.is_none() {
            match Gateway::connect(&config).await {
                Ok(mut connected) => {
                    // Older gateways may not answer; offer no GPU then.
                    let gpu = connected.gpu_supported().await.unwrap_or(false);
                    if events.send(Event::GpuSupported(gpu)).await.is_err() {
                        return;
                    }
                    // A fresh gateway knows no provider types; agents need them
                    // for their credentials.
                    if let Err(error) = connected.ensure_profiles().await {
                        let failed =
                            Event::Failed(format!("Importing provider profiles failed: {error}"));
                        if events.send(failed).await.is_err() {
                            return;
                        }
                    }
                    match accounts(&mut connected).await {
                        Ok(event) => {
                            if events.send(event).await.is_err() {
                                return;
                            }
                        }
                        Err(error) => {
                            let failed = Event::Failed(format!("Listing accounts failed: {error}"));
                            if events.send(failed).await.is_err() {
                                return;
                            }
                        }
                    }
                    gateway = Some(connected);
                }
                Err(error) => {
                    if events
                        .send(Event::Unavailable(error.to_string()))
                        .await
                        .is_err()
                    {
                        return;
                    }
                    continue;
                }
            }
        }
        let client = gateway.as_mut().expect("connected above");
        let action = command.describe();
        let sent = match execute(client, command).await {
            Ok(outcome) => {
                let mut sent = Vec::new();
                if let Some(message) = outcome.message {
                    sent.push(Event::Done(message));
                }
                // A command changes what the list shows, so follow it with one.
                let refreshed = if outcome.accounts_changed {
                    accounts(client).await.map(Some)
                } else {
                    Ok(None)
                };
                let refreshed = match refreshed {
                    Ok(accounts) => client.snapshot().await.map(|list| (accounts, list)),
                    Err(error) => Err(error),
                };
                match refreshed {
                    Ok((accounts, list)) => {
                        sent.extend(accounts);
                        sent.push(Event::Sandboxes(list));
                    }
                    Err(error) => sent.push(failure(&mut gateway, error)),
                }
                sent
            }
            Err(error) => {
                let event = failure(&mut gateway, error);
                // Say which command failed, even when the connection is what broke.
                let reason = match &event {
                    Event::Failed(reason) | Event::Unavailable(reason) => reason.clone(),
                    _ => String::new(),
                };
                match action {
                    None => vec![event],
                    Some(action) => {
                        let failed = Event::Failed(format!("{action} failed: {reason}"));
                        if matches!(event, Event::Unavailable(_)) {
                            vec![failed, event]
                        } else {
                            vec![failed]
                        }
                    }
                }
            }
        };
        for event in sent {
            if events.send(event).await.is_err() {
                return;
            }
        }
    }
}

/// An RPC status means the gateway answered and refused; anything else means
/// the connection is gone and the next tick reconnects.
fn failure(gateway: &mut Option<Gateway>, error: Error) -> Event {
    match error {
        Error::Rpc(status) if !is_connection_error(&status) => {
            // The gateway sometimes sends only a code, such as INTERNAL.
            Event::Failed(if status.message().is_empty() {
                status.code().description().to_owned()
            } else {
                status.message().to_owned()
            })
        }
        other => {
            *gateway = None;
            Event::Unavailable(other.to_string())
        }
    }
}

fn is_connection_error(status: &openshell_gateway_client::Status) -> bool {
    matches!(
        status.code(),
        openshell_gateway_client::Code::Unavailable
            | openshell_gateway_client::Code::Unknown
            | openshell_gateway_client::Code::Cancelled
    )
}

struct Outcome {
    message: Option<String>,
    accounts_changed: bool,
}

async fn accounts(gateway: &mut Gateway) -> Result<Event, Error> {
    let profiles = gateway.profiles().await?;
    let providers = gateway.providers().await?;
    Ok(Event::Accounts {
        profiles: profiles.iter().map(ProfileInfo::from_proto).collect(),
        providers,
    })
}

async fn execute(gateway: &mut Gateway, command: Command) -> Result<Outcome, Error> {
    let mut accounts_changed = false;
    let message = match command {
        Command::Refresh => None,
        Command::Create { name, options } => {
            gateway.create(&name, &options).await?;
            Some(format!("Created {name}"))
        }
        Command::Start(name) => {
            gateway.start(&name).await?;
            Some(format!("Starting {name}"))
        }
        Command::Stop(name) => {
            gateway.stop(&name).await?;
            Some(format!("Stopping {name}"))
        }
        Command::Delete(name) => {
            gateway.delete(&name).await?;
            Some(format!("Deleted {name}"))
        }
        Command::Approve { sandbox, proposal } => {
            gateway.approve(&sandbox, &proposal).await?;
            Some(format!("Allowed {}:{}", proposal.host, proposal.port))
        }
        Command::Reject { sandbox, chunk_id } => {
            gateway.reject(&sandbox, &chunk_id).await?;
            None
        }
        Command::AddRule {
            sandbox,
            host,
            port,
            binary,
        } => {
            let version = gateway
                .add_rule(&sandbox, &host, port, binary.as_deref())
                .await?;
            Some(format!("Allowed {host}:{port} (policy version {version})"))
        }
        Command::RemoveEndpoint {
            sandbox,
            rule,
            host,
            port,
        } => {
            let version = gateway
                .remove_endpoint(&sandbox, &rule, &host, port)
                .await?;
            Some(format!("Removed {host}:{port} (policy version {version})"))
        }
        Command::RemoveRule { sandbox, rule } => {
            let version = gateway.remove_rule(&sandbox, &rule).await?;
            Some(format!("Removed rule {rule} (policy version {version})"))
        }
        Command::SetAutoApprove { sandbox, enabled } => {
            gateway.set_auto_approve(&sandbox, enabled).await?;
            Some(if enabled {
                format!("{sandbox} now allows low-risk requests automatically")
            } else {
                format!("{sandbox} now asks before allowing any request")
            })
        }
        Command::CreateProvider {
            name,
            kind,
            credentials,
        } => {
            gateway.create_provider(&name, &kind, credentials.0).await?;
            accounts_changed = true;
            Some(format!("Added account {name}"))
        }
        Command::CreateCodexProvider { name, tokens } => {
            gateway.create_codex_provider(&name, &tokens).await?;
            accounts_changed = true;
            Some(format!("Added Codex account {name}"))
        }
        Command::DeleteProvider(name) => {
            gateway.delete_provider(&name).await?;
            accounts_changed = true;
            Some(format!("Removed account {name}"))
        }
    };
    Ok(Outcome {
        message,
        accounts_changed,
    })
}
