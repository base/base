//! Version information for reth.
use std::{borrow::Cow, sync::OnceLock};

use alloy_primitives::Bytes;
use base_execution_state_database::ClientVersion;

/// Global static version metadata
static VERSION_METADATA: OnceLock<RethCliVersionConsts> = OnceLock::new();

/// Initialize the global version metadata.
pub fn try_init_version_metadata(
    metadata: RethCliVersionConsts,
) -> Result<(), RethCliVersionConsts> {
    VERSION_METADATA.set(metadata)
}

/// Constants for reth-cli
///
/// Global defaults can be set via [`try_init_version_metadata`].
#[derive(Debug, Default)]
pub struct RethCliVersionConsts {
    /// The human readable name of the client
    pub name_client: Cow<'static, str>,

    /// The latest version from Cargo.toml.
    pub cargo_pkg_version: Cow<'static, str>,

    /// The short version information for reth.
    pub short_version: Cow<'static, str>,

    /// The long version information for reth.
    pub long_version: Cow<'static, str>,

    /// The version information for reth formatted for P2P (devp2p).
    ///
    /// - The latest version from Cargo.toml
    /// - The operating system and architecture
    ///
    /// # Example
    ///
    /// ```text
    /// reth/v{major}.{minor}.{patch}/{OS}-{ARCH}
    /// ```
    /// e.g.: `reth/v0.1.0/macos-aarch64`
    pub p2p_client_version: Cow<'static, str>,

    /// extra data used for payload building
    pub extra_data: Cow<'static, str>,
}

/// The default extra data used for payload building.
///
/// - The latest version from Cargo.toml
/// - The OS identifier
///
/// # Example
///
/// ```text
/// reth/v{major}.{minor}.{patch}/{OS}
/// ```
pub fn default_extra_data() -> String {
    format!("reth/v{}/{}", env!("CARGO_PKG_VERSION"), std::env::consts::OS)
}

/// The default extra data in bytes.
/// See [`default_extra_data`].
pub fn default_extra_data_bytes() -> Bytes {
    Bytes::from(default_extra_data().as_bytes().to_vec())
}

/// The default client version accessing the database.
pub fn default_client_version() -> ClientVersion {
    let meta = version_metadata();
    ClientVersion {
        version: meta.cargo_pkg_version.to_string(),
        git_sha: String::new(),
        build_timestamp: String::new(),
    }
}

/// Get a reference to the global version metadata
pub fn version_metadata() -> &'static RethCliVersionConsts {
    VERSION_METADATA.get_or_init(default_reth_version_metadata)
}

/// Default Reth version metadata from Cargo's package version.
pub fn default_reth_version_metadata() -> RethCliVersionConsts {
    RethCliVersionConsts {
        name_client: Cow::Borrowed("Reth"),
        cargo_pkg_version: Cow::Borrowed(env!("CARGO_PKG_VERSION")),
        short_version: Cow::Borrowed(env!("CARGO_PKG_VERSION")),
        long_version: Cow::Borrowed(concat!("Version: ", env!("CARGO_PKG_VERSION"))),
        p2p_client_version: Cow::Owned(format!(
            "reth/v{}/{}-{}",
            env!("CARGO_PKG_VERSION"),
            std::env::consts::OS,
            std::env::consts::ARCH,
        )),
        extra_data: Cow::Owned(default_extra_data()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assert_extra_data_less_32bytes() {
        let extra_data = default_extra_data();
        assert!(extra_data.len() <= 32, "extra data must be less than 32 bytes: {extra_data}")
    }
}
