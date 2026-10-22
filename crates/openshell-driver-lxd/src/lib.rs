//! OpenShell compute driver for LXD.
//!
//! Serves `openshell.compute.v1.ComputeDriver` on a Unix socket for the
//! OpenShell gateway and maps sandbox lifecycle onto the LXD REST API over
//! LXD's own Unix socket, so it runs inside a strict snap with the `lxd` plug.
//! See `docs/lxd-driver.md` for the design and what is not done yet.

pub mod admission;
pub mod driver;
pub mod lxd;
pub mod oci;
pub mod quantity;
pub mod service;

#[allow(clippy::all, clippy::pedantic)]
pub mod proto {
    pub mod compute {
        pub mod v1 {
            tonic::include_proto!("openshell.compute.v1");
        }
    }
    pub mod datamodel {
        pub mod v1 {
            tonic::include_proto!("openshell.datamodel.v1");
        }
    }
    pub mod extension {
        pub mod v1 {
            tonic::include_proto!("openshell.extension.v1");
        }
    }
    pub mod options {
        pub mod v1 {
            tonic::include_proto!("openshell.options.v1");
        }
    }
    pub mod sandbox {
        pub mod v1 {
            tonic::include_proto!("openshell.sandbox.v1");
        }
    }
}
