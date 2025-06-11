use alloy_primitives::{Bytes, B256};
use alloy_rpc_types_eth::erc4337::TransactionConditional;
use reth_rpc_eth_types::EthApiError;
use serde::{Deserialize, Serialize};

pub const MAX_BLOCK_RANGE_BLOCKS: u64 = 10;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Bundle {
    #[serde(rename = "txs")]
    pub transactions: Vec<Bytes>,

    #[serde(rename = "revertingTxHashes")]
    pub reverting_hashes: Option<Vec<B256>>,

    #[serde(
        default,
        rename = "maxBlockNumber",
        with = "alloy_serde::quantity::opt",
        skip_serializing_if = "Option::is_none"
    )]
    pub block_number_max: Option<u64>,

    #[serde(
        default,
        rename = "minBlockNumber",
        with = "alloy_serde::quantity::opt",
        skip_serializing_if = "Option::is_none"
    )]
    pub block_number_min: Option<u64>,

    // Not recommended because this is subject to the builder node clock
    #[serde(
        default,
        rename = "minTimestamp",
        skip_serializing_if = "Option::is_none"
    )]
    pub min_timestamp: Option<u64>,

    // Not recommended because this is subject to the builder node clock
    #[serde(
        default,
        rename = "maxTimestamp",
        skip_serializing_if = "Option::is_none"
    )]
    pub max_timestamp: Option<u64>,
}

impl From<BundleConditionalError> for EthApiError {
    fn from(err: BundleConditionalError) -> Self {
        EthApiError::InvalidParams(err.to_string())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum BundleConditionalError {
    #[error("block_number_min ({min}) is greater than block_number_max ({max})")]
    MinGreaterThanMax { min: u64, max: u64 },
    #[error("block_number_max ({max}) is a past block (current: {current})")]
    MaxBlockInPast { max: u64, current: u64 },
    #[error(
        "block_number_max ({max}) is too high (current: {current}, max allowed: {max_allowed})"
    )]
    MaxBlockTooHigh {
        max: u64,
        current: u64,
        max_allowed: u64,
    },
    #[error(
        "block_number_min ({min}) is too high with default max range (max allowed: {max_allowed})"
    )]
    MinTooHighForDefaultRange { min: u64, max_allowed: u64 },
}

impl Bundle {
    pub fn conditional(
        &self,
        last_block_number: u64,
    ) -> Result<TransactionConditional, BundleConditionalError> {
        let mut block_number_max = self.block_number_max;
        let block_number_min = self.block_number_min;

        // Validate block number ranges
        if let Some(max) = block_number_max {
            // Check if min > max
            if let Some(min) = block_number_min {
                if min > max {
                    return Err(BundleConditionalError::MinGreaterThanMax { min, max });
                }
            }

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

            // Ensure that the new max is not smaller than the min
            if let Some(min) = block_number_min {
                if min > default_max {
                    return Err(BundleConditionalError::MinTooHighForDefaultRange {
                        min,
                        max_allowed: default_max,
                    });
                }
            }
        }

        Ok(TransactionConditional {
            block_number_min,
            block_number_max,
            known_accounts: Default::default(),
            timestamp_max: self.max_timestamp,
            timestamp_min: self.min_timestamp,
        })
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct BundleResult {
    #[serde(rename = "bundleHash")]
    pub bundle_hash: B256,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bundle_conditional_no_bounds() {
        let bundle = Bundle {
            transactions: vec![],
            reverting_hashes: None,
            block_number_max: None,
            block_number_min: None,
            min_timestamp: None,
            max_timestamp: None,
        };

        let last_block = 1000;
        let result = bundle.conditional(last_block).unwrap();

        assert_eq!(result.block_number_min, None);
        assert_eq!(
            result.block_number_max,
            Some(last_block + MAX_BLOCK_RANGE_BLOCKS)
        );
    }

    #[test]
    fn test_bundle_conditional_with_valid_bounds() {
        let bundle = Bundle {
            transactions: vec![],
            reverting_hashes: None,
            block_number_max: Some(1005),
            block_number_min: Some(1002),
            min_timestamp: None,
            max_timestamp: None,
        };

        let last_block = 1000;
        let result = bundle.conditional(last_block).unwrap();

        assert_eq!(result.block_number_min, Some(1002));
        assert_eq!(result.block_number_max, Some(1005));
    }

    #[test]
    fn test_bundle_conditional_min_greater_than_max() {
        let bundle = Bundle {
            transactions: vec![],
            reverting_hashes: None,
            block_number_max: Some(1005),
            block_number_min: Some(1010),
            min_timestamp: None,
            max_timestamp: None,
        };

        let last_block = 1000;
        let result = bundle.conditional(last_block);

        assert!(matches!(
            result,
            Err(BundleConditionalError::MinGreaterThanMax {
                min: 1010,
                max: 1005
            })
        ));
    }

    #[test]
    fn test_bundle_conditional_max_in_past() {
        let bundle = Bundle {
            transactions: vec![],
            reverting_hashes: None,
            block_number_max: Some(999),
            block_number_min: None,
            min_timestamp: None,
            max_timestamp: None,
        };

        let last_block = 1000;
        let result = bundle.conditional(last_block);

        assert!(matches!(
            result,
            Err(BundleConditionalError::MaxBlockInPast {
                max: 999,
                current: 1000
            })
        ));
    }

    #[test]
    fn test_bundle_conditional_max_too_high() {
        let bundle = Bundle {
            transactions: vec![],
            reverting_hashes: None,
            block_number_max: Some(1020),
            block_number_min: None,
            min_timestamp: None,
            max_timestamp: None,
        };

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
    fn test_bundle_conditional_min_too_high_for_default_range() {
        let bundle = Bundle {
            transactions: vec![],
            reverting_hashes: None,
            block_number_max: None,
            block_number_min: Some(1015),
            min_timestamp: None,
            max_timestamp: None,
        };

        let last_block = 1000;
        let result = bundle.conditional(last_block);

        assert!(matches!(
            result,
            Err(BundleConditionalError::MinTooHighForDefaultRange {
                min: 1015,
                max_allowed: 1010
            })
        ));
    }

    #[test]
    fn test_bundle_conditional_with_only_min() {
        let bundle = Bundle {
            transactions: vec![],
            reverting_hashes: None,
            block_number_max: None,
            block_number_min: Some(1005),
            min_timestamp: None,
            max_timestamp: None,
        };

        let last_block = 1000;
        let result = bundle.conditional(last_block).unwrap();

        assert_eq!(result.block_number_min, Some(1005));
        assert_eq!(result.block_number_max, Some(1010)); // last_block + MAX_BLOCK_RANGE_BLOCKS
    }

    #[test]
    fn test_bundle_conditional_with_only_max() {
        let bundle = Bundle {
            transactions: vec![],
            reverting_hashes: None,
            block_number_max: Some(1008),
            block_number_min: None,
            min_timestamp: None,
            max_timestamp: None,
        };

        let last_block = 1000;
        let result = bundle.conditional(last_block).unwrap();

        assert_eq!(result.block_number_min, None);
        assert_eq!(result.block_number_max, Some(1008));
    }

    #[test]
    fn test_bundle_conditional_min_lower_than_last_block() {
        let bundle = Bundle {
            transactions: vec![],
            reverting_hashes: None,
            block_number_max: None,
            block_number_min: Some(999),
            min_timestamp: None,
            max_timestamp: None,
        };

        let last_block = 1000;
        let result = bundle.conditional(last_block).unwrap();

        assert_eq!(result.block_number_min, Some(999));
        assert_eq!(result.block_number_max, Some(1010));
    }
}
