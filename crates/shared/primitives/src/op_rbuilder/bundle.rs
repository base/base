use alloy_primitives::B256;
use alloy_rpc_types_eth::erc4337::TransactionConditional;
pub use base_bundles::Bundle;
use reth_rpc_eth_types::EthApiError;
use serde::{Deserialize, Serialize};

/// Maximum number of blocks allowed in the block range for bundle execution.
///
/// This constant limits how far into the future a bundle can be scheduled to
/// prevent excessive resource usage and ensure timely execution. When no
/// maximum block number is specified, this value is added to the current block
/// number to set the default upper bound.
pub const MAX_BLOCK_RANGE_BLOCKS: u64 = 10;

impl From<BundleConditionalError> for EthApiError {
    fn from(err: BundleConditionalError) -> Self {
        Self::InvalidParams(err.to_string())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum BundleConditionalError {
    #[error("block_number_min ({min}) is greater than block_number_max ({max})")]
    MinGreaterThanMax { min: u64, max: u64 },
    #[error("block_number_max ({max}) is a past block (current: {current})")]
    MaxBlockInPast { max: u64, current: u64 },
    /// To prevent resource exhaustion and ensure timely execution, bundles
    /// cannot be scheduled more than `MAX_BLOCK_RANGE_BLOCKS` blocks into the
    /// future.
    #[error(
        "block_number_max ({max}) is too high (current: {current}, max allowed: {max_allowed})"
    )]
    MaxBlockTooHigh { max: u64, current: u64, max_allowed: u64 },
    /// When no explicit maximum block number is provided, the system uses
    /// `current_block + MAX_BLOCK_RANGE_BLOCKS` as the default maximum. This
    /// error occurs when the specified minimum exceeds this default maximum.
    #[error(
        "block_number_min ({min}) is too high with default max range (max allowed: {max_allowed})"
    )]
    MinTooHighForDefaultRange { min: u64, max_allowed: u64 },
    #[error("flashblock_number_min ({min}) is greater than flashblock_number_max ({max})")]
    FlashblockMinGreaterThanMax { min: u64, max: u64 },
}

#[derive(Debug)]
pub struct BundleConditional {
    pub transaction_conditional: TransactionConditional,
    pub flashblock_number_min: Option<u64>,
    pub flashblock_number_max: Option<u64>,
}

pub trait BundleConditionalExt {
    fn conditional(
        &self,
        last_block_number: u64,
    ) -> Result<BundleConditional, BundleConditionalError>;
}

impl BundleConditionalExt for Bundle {
    fn conditional(
        &self,
        last_block_number: u64,
    ) -> Result<BundleConditional, BundleConditionalError> {
        let mut block_number_min = None;
        let mut block_number_max = None;

        if self.block_number != 0 {
            block_number_min = Some(self.block_number);
            block_number_max = Some(self.block_number);
        }

        // Validate block number ranges
        if let Some(max) = block_number_max {
            // The max block cannot be a past block
            if max <= last_block_number {
                return Err(BundleConditionalError::MaxBlockInPast {
                    max,
                    current: last_block_number,
                });
            }

            // Validate that it is not greater than the max_block_range
            let max_allowed = last_block_number + MAX_BLOCK_RANGE_BLOCKS;
            if max > max_allowed {
                return Err(BundleConditionalError::MaxBlockTooHigh {
                    max,
                    current: last_block_number,
                    max_allowed,
                });
            }
        } else {
            // If no upper bound is set, use the maximum block range
            let default_max = last_block_number + MAX_BLOCK_RANGE_BLOCKS;
            block_number_max = Some(default_max);
        }

        // Validate flashblock number range
        if let Some(min) = self.flashblock_number_min
            && let Some(max) = self.flashblock_number_max
            && min > max
        {
            return Err(BundleConditionalError::FlashblockMinGreaterThanMax { min, max });
        }

        Ok(BundleConditional {
            transaction_conditional: TransactionConditional {
                block_number_min,
                block_number_max,
                known_accounts: Default::default(),
                timestamp_max: self.max_timestamp,
                timestamp_min: self.min_timestamp,
            },
            flashblock_number_min: self.flashblock_number_min,
            flashblock_number_max: self.flashblock_number_max,
        })
    }
}

/// Result returned after successfully submitting a bundle for inclusion.
///
/// This struct contains the unique identifier for the submitted bundle, which
/// can be used to track the bundle's status and inclusion in future blocks.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct BundleResult {
    /// Transaction hash of the single transaction in the bundle.
    ///
    /// This hash can be used to:
    /// - Track bundle inclusion in blocks
    /// - Query bundle status
    /// - Reference the bundle in subsequent operations
    #[serde(rename = "bundleHash")]
    pub bundle_hash: B256,
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bundle_conditional_no_bounds() {
        let bundle = Bundle::default();

        let last_block = 1000;
        let result = bundle.conditional(last_block).unwrap().transaction_conditional;

        assert_eq!(result.block_number_min, None);
        assert_eq!(result.block_number_max, Some(last_block + MAX_BLOCK_RANGE_BLOCKS));
    }

    #[test]
    fn test_bundle_conditional_with_target_block() {
        let bundle = Bundle { block_number: 1005, ..Default::default() };

        let last_block = 1000;
        let result = bundle.conditional(last_block).unwrap().transaction_conditional;

        assert_eq!(result.block_number_min, Some(1005));
        assert_eq!(result.block_number_max, Some(1005));
    }

    #[test]
    fn test_bundle_conditional_max_in_past() {
        let bundle = Bundle { block_number: 999, ..Default::default() };

        let last_block = 1000;
        let result = bundle.conditional(last_block);

        assert!(matches!(
            result,
            Err(BundleConditionalError::MaxBlockInPast { max: 999, current: 1000 })
        ));
    }

    #[test]
    fn test_bundle_conditional_max_too_high() {
        let bundle = Bundle { block_number: 1020, ..Default::default() };

        let last_block = 1000;
        let result = bundle.conditional(last_block);

        assert!(matches!(
            result,
            Err(BundleConditionalError::MaxBlockTooHigh {
                max: 1020,
                current: 1000,
                max_allowed: 1010
            })
        ));
    }

    #[test]
    fn test_bundle_conditional_flashblock_min_greater_than_max() {
        let bundle = Bundle {
            flashblock_number_min: Some(105),
            flashblock_number_max: Some(100),
            ..Default::default()
        };

        let last_block = 1000;
        let result = bundle.conditional(last_block);

        assert!(matches!(
            result,
            Err(BundleConditionalError::FlashblockMinGreaterThanMax { min: 105, max: 100 })
        ));
    }

    #[test]
    fn test_bundle_conditional_with_valid_flashblock_range() {
        let bundle = Bundle {
            flashblock_number_min: Some(100),
            flashblock_number_max: Some(105),
            ..Default::default()
        };

        let last_block = 1000;
        let result = bundle.conditional(last_block).unwrap();

        assert_eq!(result.flashblock_number_min, Some(100));
        assert_eq!(result.flashblock_number_max, Some(105));
    }

    #[test]
    fn test_bundle_conditional_with_only_flashblock_min() {
        let bundle = Bundle { flashblock_number_min: Some(100), ..Default::default() };

        let last_block = 1000;
        let result = bundle.conditional(last_block).unwrap();

        assert_eq!(result.flashblock_number_min, Some(100));
        assert_eq!(result.flashblock_number_max, None);
    }

    #[test]
    fn test_bundle_conditional_with_only_flashblock_max() {
        let bundle = Bundle { flashblock_number_max: Some(105), ..Default::default() };

        let last_block = 1000;
        let result = bundle.conditional(last_block).unwrap();

        assert_eq!(result.flashblock_number_min, None);
        assert_eq!(result.flashblock_number_max, Some(105));
    }

    #[test]
    fn test_bundle_conditional_flashblock_equal_values() {
        let bundle = Bundle {
            flashblock_number_min: Some(100),
            flashblock_number_max: Some(100),
            ..Default::default()
        };

        let last_block = 1000;
        let result = bundle.conditional(last_block).unwrap();

        assert_eq!(result.flashblock_number_min, Some(100));
        assert_eq!(result.flashblock_number_max, Some(100));
    }
}
