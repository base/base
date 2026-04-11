//! Receipt types for Base chains.

use alloy_consensus::TxReceipt;

mod envelope;
pub use envelope::BaseReceiptEnvelope;

mod deposit;
pub use deposit::{DepositReceipt, DepositReceiptWithBloom};

mod receipt;
pub use receipt::BaseReceipt;

/// Bincode-compatible serde implementations for receipt types.
#[cfg(all(feature = "serde", feature = "serde-bincode-compat"))]
pub(super) mod serde_bincode_compat {
    pub use super::{
        deposit::serde_bincode_compat::DepositReceipt, receipt::serde_bincode_compat::BaseReceipt,
    };
}

/// Receipt is the result of a transaction execution.
pub trait BaseTxReceipt: TxReceipt {
    /// Returns the deposit nonce of the transaction.
    fn deposit_nonce(&self) -> Option<u64>;

    /// Returns the deposit receipt version of the transaction.
    fn deposit_receipt_version(&self) -> Option<u64>;
}
