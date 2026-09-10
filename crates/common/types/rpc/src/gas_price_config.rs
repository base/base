use alloy_primitives::U256;
#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

/// The default maximum number of blocks to use for the gas price oracle.
pub const MAX_HEADER_HISTORY: u64 = 1024;
/// The default maximum number of allowed reward percentiles
pub const MAX_REWARD_PERCENTILE_COUNT: u64 = 100;
/// Number of recent blocks to check for gas price
pub const DEFAULT_GAS_PRICE_BLOCKS: u32 = 20;
/// The percentile of gas prices to use for the estimate
pub const DEFAULT_GAS_PRICE_PERCENTILE: u32 = 60;
/// Maximum transaction priority fee (or gas price before London Fork) to be recommended by the
/// gas price oracle
pub const DEFAULT_MAX_GAS_PRICE: U256 = U256::from_limbs([500_000_000_000u64, 0, 0, 0]);
/// The default minimum gas price, under which the sample will be ignored
pub const DEFAULT_IGNORE_GAS_PRICE: U256 = U256::ZERO;

/// Settings for the RPC gas price oracle.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct GasPriceOracleConfig {
    /// The number of populated blocks to produce the gas price estimate
    pub blocks: u32,

    /// The percentile of gas prices to use for the estimate
    pub percentile: u32,

    /// The maximum number of headers to keep in the cache
    pub max_header_history: u64,

    /// The maximum number of blocks for estimating gas price
    pub max_block_history: u64,

    /// The maximum number for reward percentiles.
    ///
    /// This effectively limits how many transactions and receipts are fetched to compute the
    /// reward percentile.
    pub max_reward_percentile_count: u64,

    /// The default gas price to use if there are no blocks to use
    pub default_suggested_fee: Option<U256>,

    /// The maximum gas price to use for the estimate
    pub max_price: Option<U256>,

    /// The minimum gas price, under which the sample will be ignored
    pub ignore_price: Option<U256>,
}

impl Default for GasPriceOracleConfig {
    fn default() -> Self {
        Self {
            blocks: DEFAULT_GAS_PRICE_BLOCKS,
            percentile: DEFAULT_GAS_PRICE_PERCENTILE,
            max_header_history: MAX_HEADER_HISTORY,
            max_block_history: MAX_HEADER_HISTORY,
            max_reward_percentile_count: MAX_REWARD_PERCENTILE_COUNT,
            default_suggested_fee: None,
            max_price: Some(DEFAULT_MAX_GAS_PRICE),
            ignore_price: Some(DEFAULT_IGNORE_GAS_PRICE),
        }
    }
}
