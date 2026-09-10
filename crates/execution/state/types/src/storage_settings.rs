//! Persisted storage layout compatibility marker.
use serde::{Deserialize, Serialize};
/// Persisted storage layout marker. Only the v2 layout is supported.
///
/// The boolean is retained to detect incompatible databases created with storage v1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "arbitrary"), derive(arbitrary::Arbitrary))]
#[cfg_attr(feature = "reth-codec", derive(base_common_types_chain::Compact))]
pub struct StorageSettings {
    /// Whether the persisted database uses the supported v2 storage layout.
    pub storage_v2: bool,
}

impl Default for StorageSettings {
    fn default() -> Self {
        Self::v2()
    }
}

impl StorageSettings {
    /// Returns the storage layout used by Base.
    pub const fn base() -> Self {
        Self::v2()
    }

    /// Creates the supported storage layout settings.
    pub const fn v2() -> Self {
        Self { storage_v2: true }
    }
}
