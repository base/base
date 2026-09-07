//! Constants for eip-7928. Chosen to support a 630 million gas limit.

use alloy_primitives::{B256, b256};

/// Maximum number of transactions per block.
pub const MAX_TXS_PER_BLOCK: usize = 30_000;

/// Maximum number of unique storage slots modified in a block.
pub const MAX_SLOTS: usize = 300_000;

/// Maximum number of unique accounts accessed in a block.
pub const MAX_ACCOUNTS: usize = 300_000;

/// Maximum contract bytecode size in bytes.
pub const MAX_CODE_SIZE: usize = 24_576;

/// Item cost for block access list.
pub const ITEM_COST: usize = 2000;

const ETHEREUM_MAINNET_SLOTS_PER_EPOCH: u64 = 32;

/// Number of epochs the execution layer must retain block access lists for.
pub const BAL_RETENTION_PERIOD_EPOCHS: u64 = 3_533;

/// Number of slots corresponding to [`BAL_RETENTION_PERIOD_EPOCHS`].
pub const BAL_RETENTION_PERIOD_SLOTS: u64 =
    BAL_RETENTION_PERIOD_EPOCHS * ETHEREUM_MAINNET_SLOTS_PER_EPOCH;

/// The empty block access list hash.
pub const EMPTY_BLOCK_ACCESS_LIST_HASH: B256 =
    b256!("0x1dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0a142fd40d49347");
