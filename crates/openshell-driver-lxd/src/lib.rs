//! OpenShell compute driver for LXD.
//!
//! Serves `openshell.compute.v1.ComputeDriver` on a Unix socket for the
//! OpenShell gateway and maps sandbox lifecycle onto the LXD REST API over
//! LXD's own Unix socket, so it runs inside a strict snap with the `lxd` plug.
//! See `docs/lxd-driver.md` for the design and what is not done yet.

pub mod driver;
pub mod isolation;
pub mod lxd;
pub mod oci;
pub mod quantity;
pub mod service;

pub use openshell_core::proto;
