//! Transaction types for OP chains.

mod deposit;
pub use deposit::{DepositTransaction, TxDeposit};

mod tx_type;
pub use tx_type::DEPOSIT_TX_TYPE_ID;

mod envelope;
pub use envelope::{OpTransaction, OpTxEnvelope, OpTxType};

mod typed;
pub use typed::OpTypedTransaction;

mod pooled;
#[cfg(feature = "serde")]
pub use deposit::serde_deposit_tx_rpc;
pub use pooled::OpPooledTransaction;

mod meta;
pub use meta::{OpDepositInfo, OpTransactionInfo};

/// Bincode-compatible serde implementations for transaction types.
#[cfg(all(feature = "serde", feature = "serde-bincode-compat"))]
pub mod serde_bincode_compat {
    pub use super::{deposit::serde_bincode_compat::TxDeposit, envelope::serde_bincode_compat::*};
}
