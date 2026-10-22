//! Installs an agent into a running sandbox.
//!
//! The sandbox's policy keeps its /usr read-only and its network closed, so
//! the agent cannot install itself. Instead the app downloads the agent's
//! verified build on the host (see `openshell_agents`) and writes it into
//! the workload's root filesystem through LXD, at the path OpenShell's
//! provider profile for that agent allows.

use openshell_agents::{Agent, Arch};
use openshell_gateway_client::{Config, Gateway};

#[derive(Debug, Clone, PartialEq)]
pub enum InstallEvent {
    /// Bytes received so far, and the total when the server says.
    Downloading(u64, Option<u64>),
    Copying,
    /// Ready to run; `None` when it was already installed.
    Done(Option<String>),
    Failed(String),
}

/// Makes sure `agent` is installed in `sandbox`, whose workload is LXD
/// instance `instance`. Runs on its own thread; events arrive on the GTK
/// main loop through the returned channel, ending with `Done` or `Failed`.
pub fn spawn(
    agent: &'static Agent,
    sandbox: String,
    instance: String,
) -> async_channel::Receiver<InstallEvent> {
    let (events, events_rx) = async_channel::unbounded();
    std::thread::Builder::new()
        .name("agent-install".into())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("tokio runtime");
            let event = match runtime.block_on(install(agent, &sandbox, &instance, &events)) {
                Ok(version) => InstallEvent::Done(version),
                Err(error) => InstallEvent::Failed(error),
            };
            let _ = events.send_blocking(event);
        })
        .expect("spawn install thread");
    events_rx
}

async fn install(
    agent: &'static Agent,
    sandbox: &str,
    instance: &str,
    events: &async_channel::Sender<InstallEvent>,
) -> Result<Option<String>, String> {
    let mut gateway = Gateway::connect(&Config::discover())
        .await
        .map_err(|e| e.to_string())?;
    let check = vec!["test".into(), "-x".into(), agent.install_path.into()];
    if matches!(gateway.exec_status(sandbox, check).await, Ok(0)) {
        return Ok(None);
    }

    let arch = Arch::host().ok_or_else(|| {
        openshell_agents::Error::UnsupportedArch(std::env::consts::ARCH.into()).to_string()
    })?;
    let cache = gtk::glib::user_cache_dir()
        .join("openshell-desktop")
        .join("agents");
    // Report about every megabyte, not every network chunk.
    let mut reported = 0;
    let download = openshell_agents::fetch(agent, arch, &cache, |received, total| {
        if received - reported >= 1 << 20 || Some(received) == total {
            reported = received;
            let _ = events.try_send(InstallEvent::Downloading(received, total));
        }
    })
    .await
    .map_err(|e| e.to_string())?;

    let _ = events.send(InstallEvent::Copying).await;
    let path = download.path.clone();
    let size = download.size;
    let instance = instance.to_owned();
    let target = agent.install_path;
    tokio::task::spawn_blocking(move || {
        let mut file = std::fs::File::open(&path).map_err(|e| e.to_string())?;
        openshell_lxd::Client::default().push_file(&instance, target, 0o755, &mut file, size)
    })
    .await
    .map_err(|e| e.to_string())??;
    Ok(Some(download.version))
}
