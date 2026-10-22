//! `openshell-driver-lxd`: serves the OpenShell compute driver protocol on a
//! Unix socket for the gateway, backed by LXD.
//!
//! Gateway configuration:
//!
//! ```toml
//! [openshell.gateway]
//! compute_drivers = ["lxd"]
//!
//! [openshell.drivers.lxd]
//! socket_path = "/run/openshell/lxd.sock"
//! ```

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use openshell_driver_lxd::admission::AdmissionConfig;
use openshell_driver_lxd::driver::{DEFAULT_IMAGE, DriverConfig, LxdDriver};
use openshell_driver_lxd::lxd;
use openshell_driver_lxd::oci::Importer;
use openshell_driver_lxd::proto::compute::v1::compute_driver_server::ComputeDriverServer;
use openshell_driver_lxd::service::Service;
use tokio::net::UnixListener;
use tokio_stream::wrappers::UnixListenerStream;
use tracing::info;

#[derive(Parser, Debug)]
#[command(version, about)]
struct Args {
    /// Unix socket the gateway connects to.
    #[arg(
        long,
        env = "OPENSHELL_LXD_DRIVER_SOCKET",
        default_value = "/run/openshell/lxd.sock"
    )]
    socket: PathBuf,

    /// Permission bits for the driver socket. Only the gateway's user should
    /// be able to connect.
    #[arg(long, default_value = "600", value_parser = parse_mode)]
    socket_mode: u32,

    /// LXD daemon socket.
    #[arg(long, env = "LXD_SOCKET", default_value = lxd::SNAP_SOCKET)]
    lxd_socket: PathBuf,

    /// LXD project that holds sandbox instances and images.
    #[arg(long, env = "OPENSHELL_LXD_PROJECT", default_value = "default")]
    project: String,

    /// LXD storage pool for sandbox root disks.
    #[arg(long, env = "OPENSHELL_LXD_STORAGE_POOL", default_value = "default")]
    storage_pool: String,

    /// Image used when a sandbox does not name one.
    #[arg(long, default_value = DEFAULT_IMAGE)]
    default_image: String,

    /// Where OCI layers are staged while being converted to LXD images.
    #[arg(long, env = "OPENSHELL_LXD_STAGING_DIR")]
    staging_dir: Option<PathBuf>,

    /// Resource admission policy; must match the gateway's
    /// `[openshell.drivers.lxd]` policy. `{}` uses OpenShell's defaults.
    #[arg(
        long,
        env = "OPENSHELL_DRIVER_ADMISSION_CONFIG_JSON",
        default_value = "{}"
    )]
    admission_config_json: String,

    /// GPU passed to sandboxes that request one: an LXD GPU `id`, or
    /// `pci:<address>`. GPU requests are refused when unset.
    #[arg(long)]
    gpu: Option<String>,

    /// Set `nvidia.runtime=true` on GPU sandboxes.
    #[arg(long)]
    nvidia_runtime: bool,
}

fn parse_mode(value: &str) -> Result<u32, String> {
    u32::from_str_radix(value, 8).map_err(|e| format!("invalid octal mode {value:?}: {e}"))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let args = Args::parse();

    let staging_dir = args.staging_dir.unwrap_or_else(|| {
        std::env::var_os("SNAP_COMMON")
            .map(|common| PathBuf::from(common).join("staging"))
            .unwrap_or_else(|| std::env::temp_dir().join("openshell-driver-lxd"))
    });
    let config = DriverConfig {
        storage_pool: args.storage_pool,
        default_image: args.default_image,
        admission: AdmissionConfig::parse(&args.admission_config_json)?,
        gpu: args.gpu,
        nvidia_runtime: args.nvidia_runtime,
        ..DriverConfig::default()
    };

    let client = lxd::Client::new(&args.lxd_socket, args.project);
    let server = client.server().await?;
    info!(
        lxd = %args.lxd_socket.display(),
        version = server["environment"]["server_version"].as_str().unwrap_or("unknown"),
        project = client.project(),
        "connected to LXD"
    );

    let driver = LxdDriver::new(client, Arc::new(Importer::new(staging_dir)), config);
    tokio::spawn(driver.clone().pump_events());

    if let Some(parent) = args.socket.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match std::fs::remove_file(&args.socket) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    let listener = UnixListener::bind(&args.socket)?;
    std::fs::set_permissions(
        &args.socket,
        std::fs::Permissions::from_mode(args.socket_mode),
    )?;
    info!(socket = %args.socket.display(), "serving OpenShell compute driver");

    tonic::transport::Server::builder()
        .add_service(ComputeDriverServer::new(Service::new(driver)))
        .serve_with_incoming_shutdown(UnixListenerStream::new(listener), async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    let _ = std::fs::remove_file(&args.socket);
    Ok(())
}
