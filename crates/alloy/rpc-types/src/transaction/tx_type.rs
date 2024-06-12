//! OP transaction identifiers.

use serde::{Deserialize, Serialize};

/// Identifier for an Optimism deposit transaction
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
            TxType::Legacy => 0,
            TxType::Eip2930 => 1,
            TxType::Eip1559 => 2,
            TxType::Eip4844 => 3,
            TxType::Deposit => DEPOSIT_TX_TYPE_ID,
        }
    }
}
