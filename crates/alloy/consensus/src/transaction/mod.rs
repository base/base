//! Tramsaction types for Optimism.

mod deposit;
pub use deposit::TxDeposit;

mod tx_type;
pub use tx_type::{OpTxType, DEPOSIT_TX_TYPE_ID};

mod envelope;
pub use envelope::OpTxEnvelope;

mod typed;
pub use typed::OpTypedTransaction;

mod source;
pub use source::{
    DepositSourceDomain, DepositSourceDomainIdentifier, L1InfoDepositSource, UpgradeDepositSource,
    UserDepositSource,
};

#[cfg(feature = "serde")]
pub use deposit::serde_deposit_tx_rpc;

/// Bincode-compatible serde implementations for transaction types.
#[cfg(all(feature = "serde", feature = "serde-bincode-compat"))]
pub(super) mod serde_bincode_compat {
    pub use super::deposit::serde_bincode_compat::TxDeposit;
}

use alloy_primitives::B256;

/// A trait representing a deposit transaction with specific attributes.
pub trait DepositTransaction {
    /// Returns the hash that uniquely identifies the source of the deposit.
    ///
    /// # Returns
    /// An `Option<B256>` containing the source hash if available.
    fn source_hash(&self) -> Option<B256>;

    /// Returns the optional mint value of the deposit transaction.
    ///
    /// # Returns
    /// An `Option<u128>` representing the ETH value to mint on L2, if any.
    fn mint(&self) -> Option<u128>;

    /// Indicates whether the transaction is exempt from the L2 gas limit.
    ///
    /// # Returns
    /// A `bool` indicating if the transaction is a system transaction.
    fn is_system_transaction(&self) -> bool;

    /// Checks if the transaction is a deposit transaction.
    ///
    /// # Returns
    /// A `bool` that is always `true` for deposit transactions.
    fn is_deposit(&self) -> bool;
}
