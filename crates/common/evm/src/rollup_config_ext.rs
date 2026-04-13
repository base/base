//! Extension trait for [`RollupConfig`] providing revm-specific utilities.

use base_consensus_genesis::RollupConfig;

use crate::OpSpecId;

/// Extension trait for [`RollupConfig`] providing revm-specific utilities.
pub trait RollupConfigExt {
    /// Returns the active [`OpSpecId`] for the given `timestamp`.
    fn spec_id(&self, timestamp: u64) -> OpSpecId;
}

impl RollupConfigExt for RollupConfig {
    fn spec_id(&self, timestamp: u64) -> OpSpecId {
        if self.is_base_v1_active(timestamp) {
            OpSpecId::BASE_V1
        } else if self.is_jovian_active(timestamp) {
            OpSpecId::JOVIAN
        } else if self.is_isthmus_active(timestamp) {
            OpSpecId::ISTHMUS
        } else if self.is_holocene_active(timestamp) {
            OpSpecId::HOLOCENE
        } else if self.is_fjord_active(timestamp) {
            OpSpecId::FJORD
        } else if self.is_ecotone_active(timestamp) {
            OpSpecId::ECOTONE
        } else if self.is_canyon_active(timestamp) {
            OpSpecId::CANYON
        } else if self.is_regolith_active(timestamp) {
            OpSpecId::REGOLITH
        } else {
            OpSpecId::BEDROCK
        }
    }
}

#[cfg(test)]
mod tests {
    use base_consensus_genesis::{HardForkConfig, HardforkConfig, RollupConfig};

    use super::*;

    #[test]
    fn test_spec_id() {
        let mut config = RollupConfig {
            hardforks: HardForkConfig { regolith_time: Some(10), ..Default::default() },
            ..Default::default()
        };
        assert_eq!(config.spec_id(0), OpSpecId::BEDROCK);
        assert_eq!(config.spec_id(10), OpSpecId::REGOLITH);
        config.hardforks.canyon_time = Some(20);
        assert_eq!(config.spec_id(20), OpSpecId::CANYON);
        config.hardforks.ecotone_time = Some(30);
        assert_eq!(config.spec_id(30), OpSpecId::ECOTONE);
        config.hardforks.fjord_time = Some(40);
        assert_eq!(config.spec_id(40), OpSpecId::FJORD);
        config.hardforks.holocene_time = Some(50);
        assert_eq!(config.spec_id(50), OpSpecId::HOLOCENE);
        config.hardforks.isthmus_time = Some(60);
        assert_eq!(config.spec_id(60), OpSpecId::ISTHMUS);
        config.hardforks.jovian_time = Some(65);
        assert_eq!(config.spec_id(65), OpSpecId::JOVIAN);
        config.hardforks.base = HardforkConfig { v1: Some(70) };
        assert_eq!(config.spec_id(70), OpSpecId::BASE_V1);
        // V1 takes precedence over Jovian when both are active at the same timestamp
        config.hardforks.base = HardforkConfig { v1: Some(65) };
        assert_eq!(config.spec_id(65), OpSpecId::BASE_V1);
    }
}
