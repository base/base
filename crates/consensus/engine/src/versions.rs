//! Engine API version selection based on Base upgrade activations.
//!
//! Automatically selects the appropriate Engine API method versions based on
//! the rollup configuration and block timestamps. Different Base upgrades
//! require different Engine API versions to support new features.
//!
//! # Version Mapping
//!
//! - **Bedrock, Canyon, Delta** → V2 methods
//! - **Ecotone (Cancun)** → V3 methods
//! - **Isthmus, Jovian** → V4 methods
//! - **Base Azul (Osaka)** → `getPayloadV5`, `newPayloadV4`
//! - **Denim (Amsterdam)** → `forkchoiceUpdatedV4`, `newPayloadV5`, `getPayloadV6`
//!
//! Adapted from the [reference node version providers](https://github.com/ethereum-optimism/optimism/blob/develop/op-node/rollup/types.go#L546).

use base_common_genesis::RollupConfig;

/// Engine API version for `engine_forkchoiceUpdated` method calls.
///
/// Selects between V2, V3, and V4 based on upgrade activation. V3 is required
/// for Ecotone/Cancun, and V4 for Denim/Amsterdam, which adds the EIP-7843
/// `slotNumber` to the payload attributes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EngineForkchoiceVersion {
    /// Version 2: Used for Bedrock, Canyon, and Delta upgrades.
    V2,
    /// Version 3: Required for Ecotone/Cancun and later upgrades.
    V3,
    /// Version 4: Required for Denim/Amsterdam, which carries the slot number.
    V4,
}

impl EngineForkchoiceVersion {
    /// Returns the appropriate [`EngineForkchoiceVersion`] for the chain at the given attributes.
    ///
    /// Uses the [`RollupConfig`] to check which upgrade is active at the given timestamp.
    pub fn from_cfg(cfg: &RollupConfig, timestamp: u64) -> Self {
        if cfg.is_denim_active(timestamp) {
            // Amsterdam
            Self::V4
        } else if cfg.is_ecotone_active(timestamp) {
            // Cancun
            Self::V3
        } else {
            // Bedrock, Canyon, Delta
            Self::V2
        }
    }
}

/// Engine API version for `engine_newPayload` method calls.
///
/// Progressive version selection based on upgrade activation:
/// - V2: Basic payload processing
/// - V3: Adds Cancun/Ecotone support
/// - V4: Adds Isthmus upgrade support (also used for Jovian and Base Azul)
/// - V5: Adds the Amsterdam block access list and slot number (Denim)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EngineNewPayloadVersion {
    /// Version 2: Basic payload processing for early upgrades.
    V2,
    /// Version 3: Adds Cancun/Ecotone consensus features.
    V3,
    /// Version 4: Adds Isthmus upgrade support.
    V4,
    /// Version 5: Adds Amsterdam block access list and slot number support.
    V5,
}

impl EngineNewPayloadVersion {
    /// Returns the appropriate [`EngineNewPayloadVersion`] for the chain at the given timestamp.
    ///
    /// Uses the [`RollupConfig`] to check which upgrade is active at the given timestamp.
    pub fn from_cfg(cfg: &RollupConfig, timestamp: u64) -> Self {
        if cfg.is_denim_active(timestamp) {
            // Amsterdam
            Self::V5
        } else if cfg.is_isthmus_active(timestamp) {
            Self::V4
        } else if cfg.is_ecotone_active(timestamp) {
            // Cancun
            Self::V3
        } else {
            Self::V2
        }
    }
}

/// Engine API version for `engine_getPayload` method calls.
///
/// Matches the payload version used for retrieval with the version
/// used during payload construction, ensuring API compatibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EngineGetPayloadVersion {
    /// Version 2: Basic payload retrieval.
    V2,
    /// Version 3: Enhanced payload data for Cancun/Ecotone.
    V3,
    /// Version 4: Extended payload format for Isthmus.
    V4,
    /// Version 5: Osaka payload retrieval for Base Azul.
    V5,
    /// Version 6: Amsterdam payload retrieval for Denim, which carries the block access list
    /// and slot number.
    V6,
}

impl EngineGetPayloadVersion {
    /// Returns the appropriate [`EngineGetPayloadVersion`] for the chain at the given timestamp.
    ///
    /// Uses the [`RollupConfig`] to check which upgrade is active at the given timestamp.
    pub fn from_cfg(cfg: &RollupConfig, timestamp: u64) -> Self {
        if cfg.is_denim_active(timestamp) {
            Self::V6
        } else if cfg.is_base_azul_active(timestamp) {
            Self::V5
        } else if cfg.is_isthmus_active(timestamp) {
            Self::V4
        } else if cfg.is_ecotone_active(timestamp) {
            // Cancun
            Self::V3
        } else {
            Self::V2
        }
    }
}

#[cfg(test)]
mod tests {
    use base_common_genesis::{BaseUpgradeConfig, UpgradeConfig};

    use super::*;

    fn rollup_config_with_denim(denim_time: u64) -> RollupConfig {
        RollupConfig {
            upgrades: UpgradeConfig {
                ecotone_time: Some(20),
                jovian_time: Some(30),
                base: BaseUpgradeConfig {
                    azul: Some(40),
                    beryl: Some(50),
                    cobalt: Some(60),
                    denim: Some(denim_time),
                    everest: None,
                    zenith: None,
                },
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn test_rollup_config() -> RollupConfig {
        RollupConfig {
            upgrades: UpgradeConfig {
                ecotone_time: Some(20),
                jovian_time: Some(30),
                base: BaseUpgradeConfig {
                    azul: Some(40),
                    beryl: Some(50),
                    cobalt: Some(60),
                    denim: None,
                    everest: None,
                    zenith: None,
                },
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn get_payload_version_uses_v2_before_ecotone() {
        assert_eq!(
            EngineGetPayloadVersion::from_cfg(&test_rollup_config(), 10),
            EngineGetPayloadVersion::V2
        );
    }

    #[test]
    fn get_payload_version_uses_v3_for_ecotone() {
        assert_eq!(
            EngineGetPayloadVersion::from_cfg(&test_rollup_config(), 25),
            EngineGetPayloadVersion::V3
        );
    }

    #[test]
    fn get_payload_version_uses_v4_for_jovian_before_azul() {
        assert_eq!(
            EngineGetPayloadVersion::from_cfg(&test_rollup_config(), 35),
            EngineGetPayloadVersion::V4
        );
    }

    #[test]
    fn get_payload_version_uses_v5_for_azul() {
        assert_eq!(
            EngineGetPayloadVersion::from_cfg(&test_rollup_config(), 45),
            EngineGetPayloadVersion::V5
        );
    }

    #[test]
    fn denim_selects_the_amsterdam_engine_api_versions() {
        let cfg = rollup_config_with_denim(70);

        assert_eq!(
            EngineForkchoiceVersion::from_cfg(&cfg, 70),
            EngineForkchoiceVersion::V4,
            "Denim must use forkchoiceUpdatedV4"
        );
        assert_eq!(
            EngineNewPayloadVersion::from_cfg(&cfg, 70),
            EngineNewPayloadVersion::V5,
            "Denim must use newPayloadV5"
        );
        assert_eq!(
            EngineGetPayloadVersion::from_cfg(&cfg, 70),
            EngineGetPayloadVersion::V6,
            "Denim must use getPayloadV6"
        );

        // The block before Denim keeps the pre-Amsterdam methods.
        assert_eq!(
            EngineForkchoiceVersion::from_cfg(&cfg, 69),
            EngineForkchoiceVersion::V3,
            "pre-Denim must keep forkchoiceUpdatedV3"
        );
        assert_eq!(
            EngineNewPayloadVersion::from_cfg(&cfg, 69),
            EngineNewPayloadVersion::V4,
            "pre-Denim must keep newPayloadV4"
        );
        assert_eq!(
            EngineGetPayloadVersion::from_cfg(&cfg, 69),
            EngineGetPayloadVersion::V5,
            "pre-Denim must keep getPayloadV5"
        );
    }
}
