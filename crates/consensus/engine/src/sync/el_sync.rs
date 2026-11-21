//! Offset anchor selection after execution-layer synchronization.

use base_common_genesis::RollupConfig;

/// Selects an EL-sync derivation anchor using the rollup's timestamp schedule.
#[derive(Debug)]
pub struct ElSyncAnchor;

impl ElSyncAnchor {
    /// Default lookback matching op-node's twelve-hour EL-safe offset.
    pub const DEFAULT_OFFSET_SECONDS: u64 = 43_200;

    /// Finds the last block at or before the offset timestamp, bounded by genesis.
    pub fn block_number(config: &RollupConfig, head: u64, offset_seconds: u64) -> u64 {
        let target = config.l2_block_timestamp_millis(head)
            .saturating_sub(offset_seconds.saturating_mul(1_000));
        let mut low = config.genesis.l2.number;
        let mut high = head.max(low);
        while low < high {
            let middle = low + (high - low) / 2 + (high - low) % 2;
            if config.l2_block_timestamp_millis(middle) <= target {
                low = middle;
            } else {
                high = middle - 1;
            }
        }
        low
    }
}

#[cfg(test)]
mod tests {
    use base_common_genesis::{BaseUpgradeConfig, UpgradeConfig};

    use super::*;

    /// Offset selection must account for the legacy-to-200ms transition.
    #[test]
    fn offset_spans_denim() {
        let config = RollupConfig {
            block_time: 2,
            upgrades: UpgradeConfig {
                base: BaseUpgradeConfig { denim: Some(40_000), ..Default::default() },
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(ElSyncAnchor::block_number(&config, 40_000, 43_200), 400, "12h before the 44000s head is legacy block400");
        assert_eq!(ElSyncAnchor::block_number(&config, 40_000, 0), 40_000, "zero offset selects the tip");
        assert_eq!(ElSyncAnchor::block_number(&config, 40_000, 100_000), 0, "offset is bounded by configured genesis");
    }
}
