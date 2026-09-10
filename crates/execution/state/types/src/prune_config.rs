use crate::{MINIMUM_UNWIND_SAFE_DISTANCE, PruneModes};

/// The default interval between pruning runs, in blocks.
pub const DEFAULT_BLOCK_INTERVAL: usize = 5;

/// Pruning configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(default))]
pub struct PruneConfig {
    /// Maximum rows deleted per pruning run. Defaults to no row limit.
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub delete_limit: Option<usize>,
    /// Minimum pruning interval measured in blocks.
    pub block_interval: usize,
    /// Pruning configuration for every part of the data that can be pruned.
    #[cfg_attr(feature = "serde", serde(alias = "parts"))]
    pub segments: PruneModes,
    /// Minimum distance from the tip required for pruning. Controls the safety margin for
    /// reorgs and manual unwinds. Defaults to [`MINIMUM_UNWIND_SAFE_DISTANCE`].
    #[cfg_attr(
        feature = "serde",
        serde(default = "PruneConfig::default_minimum_pruning_distance")
    )]
    pub minimum_pruning_distance: u64,
}

impl Default for PruneConfig {
    fn default() -> Self {
        Self {
            delete_limit: None,
            block_interval: DEFAULT_BLOCK_INTERVAL,
            segments: PruneModes::default(),
            minimum_pruning_distance: MINIMUM_UNWIND_SAFE_DISTANCE,
        }
    }
}

impl PruneConfig {
    /// Default minimum distance from the tip required for pruning.
    pub const fn default_minimum_pruning_distance() -> u64 {
        MINIMUM_UNWIND_SAFE_DISTANCE
    }

    /// Returns whether this configuration is the default one.
    pub fn is_default(&self) -> bool {
        self == &Self::default()
    }

    /// Returns whether there is any kind of receipt pruning configuration.
    pub fn has_receipts_pruning(&self) -> bool {
        self.segments.has_receipts_pruning()
    }

    /// Merges values from `other` into `self`.
    /// - `Option<PruneMode>` fields: set from `other` only if `self` is `None`.
    /// - `block_interval`: set from `other` only if `self.block_interval ==
    ///   DEFAULT_BLOCK_INTERVAL`.
    /// - `receipts_log_filter`: set from `other` only if `self` is empty and `other` is non-empty.
    pub fn merge(&mut self, other: Self) {
        if self.delete_limit.is_none() {
            self.delete_limit = other.delete_limit;
        }

        // Merge block_interval, only update if it's the default interval
        if self.block_interval == DEFAULT_BLOCK_INTERVAL {
            self.block_interval = other.block_interval;
        }

        // Merge minimum_pruning_distance, only update if it's the default
        if self.minimum_pruning_distance == MINIMUM_UNWIND_SAFE_DISTANCE {
            self.minimum_pruning_distance = other.minimum_pruning_distance;
        }

        // Merge the various segment prune modes
        self.segments.sender_recovery =
            self.segments.sender_recovery.or(other.segments.sender_recovery);
        self.segments.transaction_lookup =
            self.segments.transaction_lookup.or(other.segments.transaction_lookup);
        self.segments.receipts = self.segments.receipts.or(other.segments.receipts);
        self.segments.account_history =
            self.segments.account_history.or(other.segments.account_history);
        self.segments.storage_history =
            self.segments.storage_history.or(other.segments.storage_history);
        self.segments.bodies_history =
            self.segments.bodies_history.or(other.segments.bodies_history);

        if self.segments.receipts_log_filter.0.is_empty()
            && !other.segments.receipts_log_filter.0.is_empty()
        {
            self.segments.receipts_log_filter = other.segments.receipts_log_filter;
        }
    }
}

#[cfg(test)]
mod tests {
    use alloc::collections::BTreeMap;

    use alloy_primitives::Address;

    use crate::{
        MINIMUM_UNWIND_SAFE_DISTANCE, PruneConfig, PruneMode, PruneModes, ReceiptsLogPruneConfig,
    };
    #[test]
    fn test_prune_config_merge() {
        let mut config1 = PruneConfig {
            delete_limit: None,
            block_interval: 5,
            minimum_pruning_distance: MINIMUM_UNWIND_SAFE_DISTANCE,
            segments: PruneModes {
                sender_recovery: Some(PruneMode::Full),
                transaction_lookup: None,
                receipts: Some(PruneMode::Distance(1000)),
                account_history: None,
                storage_history: Some(PruneMode::Before(5000)),
                bodies_history: None,
                receipts_log_filter: ReceiptsLogPruneConfig(BTreeMap::from([(
                    Address::repeat_byte(1),
                    PruneMode::Full,
                )])),
            },
        };

        let config2 = PruneConfig {
            delete_limit: Some(20_000),
            block_interval: 10,
            minimum_pruning_distance: MINIMUM_UNWIND_SAFE_DISTANCE,
            segments: PruneModes {
                sender_recovery: Some(PruneMode::Distance(500)),
                transaction_lookup: Some(PruneMode::Full),
                receipts: Some(PruneMode::Full),
                account_history: Some(PruneMode::Distance(2000)),
                storage_history: Some(PruneMode::Distance(3000)),
                bodies_history: None,
                receipts_log_filter: ReceiptsLogPruneConfig(BTreeMap::from([
                    (Address::repeat_byte(2), PruneMode::Distance(1000)),
                    (Address::repeat_byte(3), PruneMode::Before(2000)),
                ])),
            },
        };

        let original_filter = config1.segments.receipts_log_filter.clone();
        config1.merge(config2);

        // Check that the configuration has been merged. Any configuration present in config1
        // should not be overwritten by config2
        assert_eq!(config1.block_interval, 10);
        assert_eq!(config1.delete_limit, Some(20_000));
        assert_eq!(config1.segments.sender_recovery, Some(PruneMode::Full));
        assert_eq!(config1.segments.transaction_lookup, Some(PruneMode::Full));
        assert_eq!(config1.segments.receipts, Some(PruneMode::Distance(1000)));
        assert_eq!(config1.segments.account_history, Some(PruneMode::Distance(2000)));
        assert_eq!(config1.segments.storage_history, Some(PruneMode::Before(5000)));
        assert_eq!(config1.segments.receipts_log_filter, original_filter);
    }
}
