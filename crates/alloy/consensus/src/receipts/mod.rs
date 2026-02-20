//! Receipt types for OP chains.

use alloy_consensus::TxReceipt;

mod envelope;
pub use envelope::OpReceiptEnvelope;

mod deposit;
pub use deposit::{OpDepositReceipt, OpDepositReceiptWithBloom};

mod receipt;
pub use receipt::OpReceipt;

/// Bincode-compatible serde implementations for receipt types.
#[cfg(all(feature = "serde", feature = "serde-bincode-compat"))]
pub mod serde_bincode_compat {
    pub use super::{
        deposit::serde_bincode_compat::OpDepositReceipt,
        receipt::serde_bincode_compat::OpReceipt,
    };
}

/// Receipt is the result of a transaction execution.
pub trait OpTxReceipt: TxReceipt {
    /// Returns the deposit nonce of the transaction.
    fn deposit_nonce(&self) -> Option<u64>;

    /// Returns the deposit receipt version of the transaction.
    fn deposit_receipt_version(&self) -> Option<u64>;
}
