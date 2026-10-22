//! Background connection to the OpenShell gateway.
//!
//! The gateway client is async, the UI is not. A dedicated thread runs a tokio
//! runtime, polls the sandbox list and executes commands; the window sends
//! `Command`s in and receives `Event`s on the GTK main loop.

use std::time::Duration;

use openshell_gateway_client::{Config, CreateOptions, Error, Gateway, Proposal, Snapshot};

const POLL: Duration = Duration::from_secs(3);

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
                Ok(connected) => gateway = Some(connected),
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
        let sent = match execute(client, command).await {
            Ok(outcome) => {
                let mut sent = Vec::new();
                if let Some(message) = outcome.message {
                    sent.push(Event::Done(message));
                }
                // A command changes what the list shows, so follow it with one.
                match client.snapshot().await {
                    Ok(list) => sent.push(Event::Sandboxes(list)),
                    Err(error) => sent.push(failure(&mut gateway, error)),
                }
                sent
            }
            Err(error) => vec![failure(&mut gateway, error)],
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
            Event::Failed(status.message().into())
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
}

async fn execute(gateway: &mut Gateway, command: Command) -> Result<Outcome, Error> {
    let message = match command {
        Command::Refresh => None,
        Command::Create { name, options } => {
            gateway.create(&name, &options).await?;
            Some(format!("Creating {name}"))
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
    };
    Ok(Outcome { message })
}
