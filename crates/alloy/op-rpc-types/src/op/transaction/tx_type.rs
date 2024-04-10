use alloy_primitives::U8;
use serde::{Deserialize, Serialize};

/// Identifier for legacy transaction, however a legacy tx is technically not
/// typed.
pub const LEGACY_TX_TYPE_ID: u8 = 0;

/// Identifier for an EIP2930 transaction.
pub const EIP2930_TX_TYPE_ID: u8 = 1;

/// Identifier for an EIP1559 transaction.
pub const EIP1559_TX_TYPE_ID: u8 = 2;

/// Identifier for an EIP4844 transaction.
pub const EIP4844_TX_TYPE_ID: u8 = 3;

// Identifier for an Optimism deposit transaction
pub const DEPOSIT_TX_TYPE_ID: u8 = 126;

/// Transaction Type
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize, Hash,
)]
pub enum TxType {
    /// Legacy transaction pre EIP-2929
    #[default]
    Legacy = 0_isize,
    /// AccessList transaction
    Eip2930 = 1_isize,
    /// Transaction with Priority fee
    Eip1559 = 2_isize,
    /// Shard Blob Transactions - EIP-4844
    Eip4844 = 3_isize,
    /// Optimism Deposit transaction.
    Deposit = 126_isize,
}

impl From<TxType> for u8 {
    fn from(value: TxType) -> Self {
        match value {
            TxType::Legacy => LEGACY_TX_TYPE_ID,
            TxType::EIP2930 => EIP2930_TX_TYPE_ID,
            TxType::EIP1559 => EIP1559_TX_TYPE_ID,
            TxType::EIP4844 => EIP4844_TX_TYPE_ID,
            TxType::Deposit => DEPOSIT_TX_TYPE_ID
        }
    }
}
