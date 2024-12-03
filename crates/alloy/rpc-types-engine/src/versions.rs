//! Versions for the engine api.

use op_alloy_genesis::RollupConfig;

/// The version of the engine api.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u32)]
pub enum ForkchoiceUpdateVersion {
    /// Version 1 of the engine api.
    V1 = 1,
    /// Version 2 of the engine api.
    V2 = 2,
    /// Version 3 of the engine api.
    V3 = 3,
    /// Version 4 of the engine api.
    V4 = 4,
}

impl ForkchoiceUpdateVersion {
    /// Constructs a `ForkchoiceUpdateVersion` from a [RollupConfig] and timestamp.
    ///
    /// See: <https://github.com/ethereum-optimism/optimism/blob/4ee839ae8996c2d421a2d85fd5471897840014fa/op-node/rollup/types.go#L514>
    pub fn from_config(config: &RollupConfig, timestamp: u64) -> Self {
        if config.is_ecotone_active(timestamp) {
            // Cancun
            Self::V3
        } else if config.is_canyon_active(timestamp) {
            // Shanghai
            Self::V2
        } else {
            // According to Ethereum engine API spec, we can use fcuV2 here,
            // but upstream Geth v1.13.11 does not accept V2 before Shanghai.
            Self::V1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fcu_from_config_ecotone() {
        let config =
            RollupConfig { ecotone_time: Some(10), canyon_time: Some(5), ..Default::default() };
        let expected = ForkchoiceUpdateVersion::V3;
        assert_eq!(ForkchoiceUpdateVersion::from_config(&config, 10), expected);
        let expected = ForkchoiceUpdateVersion::V2;
        assert_eq!(ForkchoiceUpdateVersion::from_config(&config, 5), expected);
        let expected = ForkchoiceUpdateVersion::V1;
        assert_eq!(ForkchoiceUpdateVersion::from_config(&config, 0), expected);
    }

    #[test]
    fn test_fcu_from_config_canyon() {
        let config = RollupConfig { canyon_time: Some(1), ..Default::default() };
        let expected = ForkchoiceUpdateVersion::V2;
        assert_eq!(ForkchoiceUpdateVersion::from_config(&config, 1), expected);
        let expected = ForkchoiceUpdateVersion::V1;
        assert_eq!(ForkchoiceUpdateVersion::from_config(&config, 0), expected);
    }

    #[test]
    fn test_fcu_from_config() {
        let config = RollupConfig::default();
        let expected = ForkchoiceUpdateVersion::V1;
        assert_eq!(ForkchoiceUpdateVersion::from_config(&config, 0), expected);
    }
}
