//! Contains node versioning info.

use reth_node_core::version::{
    RethCliVersionConsts, default_reth_version_metadata, try_init_version_metadata,
};

/// Encapsulates versioning.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct Version;

impl Version {
    /// The client version string for the Base Reth node.
    pub const NODE_RETH_CLIENT_VERSION: &str = concat!("base/v", env!("CARGO_PKG_VERSION"));

    /// Initializes the versioning for the Base Reth node.
    ///
    /// ### Panics
    ///
    /// Panics if unable to initialize version metadata.
    pub fn init() {
        let default_version_metadata = default_reth_version_metadata();
        try_init_version_metadata(RethCliVersionConsts {
            name_client: "Base Reth Node".to_string().into(),
            cargo_pkg_version: format!(
                "{}/{}",
                default_version_metadata.cargo_pkg_version,
                env!("CARGO_PKG_VERSION")
            )
            .into(),
            p2p_client_version: format!(
                "{}/{}",
                default_version_metadata.p2p_client_version,
                Self::NODE_RETH_CLIENT_VERSION
            )
            .into(),
            extra_data: format!(
                "{}/{}",
                default_version_metadata.extra_data,
                Self::NODE_RETH_CLIENT_VERSION
            )
            .into(),
            ..default_version_metadata
        })
        .expect("Unable to init version metadata");
    }
}
