//! Contains node versioning info.

use metrics::gauge;
use reth_node_core::version::{
    RethCliVersionConsts, default_reth_version_metadata, try_init_version_metadata,
};

/// Encapsulates versioning utilities for Base binaries.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct Version;

impl Version {
    /// Initializes Reth's global version metadata using the binary's package info.
    ///
    /// This sets up the client name, P2P version string, and extra data fields
    /// that Reth uses for network identification and logging.
    ///
    /// ### Panics
    ///
    /// Panics if unable to initialize version metadata.
    pub fn init_reth(version: &'static str, pkg_name: &'static str) {
        let default = default_reth_version_metadata();
        let client_version = format!("base/v{version}");

        try_init_version_metadata(RethCliVersionConsts {
            name_client: pkg_name.to_string().into(),
            cargo_pkg_version: format!("{}/{}", default.cargo_pkg_version, version).into(),
            p2p_client_version: format!("{}/{}", default.p2p_client_version, client_version).into(),
            extra_data: format!("{}/{}", default.extra_data, client_version).into(),
            ..default
        })
        .expect("Unable to init version metadata");
    }

    /// Exposes version information over Prometheus as `base_info{version="..."}`.
    pub fn register_metrics(version: &'static str) {
        let labels: [(&str, &str); 1] = [("version", version)];
        let gauge = gauge!("base_info", &labels);
        gauge.set(1);
    }
}

/// Initializes Reth's global version metadata.
///
/// Use this in execution layer binaries (base-node-reth, op-rbuilder) that need
/// Reth's global version metadata initialized for P2P identification and logging.
///
/// This macro must be called from the binary crate to capture the correct package metadata.
#[macro_export]
macro_rules! init_reth_version {
    () => {
        $crate::Version::init_reth(env!("CARGO_PKG_VERSION"), env!("CARGO_PKG_NAME"))
    };
}

/// Registers version information as Prometheus metrics (`base_info{version="..."}`).
#[macro_export]
macro_rules! register_version_metrics {
    () => {
        $crate::Version::register_metrics(env!("CARGO_PKG_VERSION"))
    };
}
