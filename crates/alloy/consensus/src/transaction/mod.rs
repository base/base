mod deposit;
pub use deposit::TxDeposit;

mod envelope;
pub use envelope::{OpTxEnvelope, OpTxType, DEPOSIT_TX_TYPE_ID};

mod typed;
pub use typed::OpTypedTransaction;

mod source;
pub use source::{
    DepositSourceDomain, DepositSourceDomainIdentifier, L1InfoDepositSource, UpgradeDepositSource,
    UserDepositSource,
};

/// Bincode-compatible serde implementations for transaction types.
#[cfg(all(feature = "serde", feature = "serde-bincode-compat"))]
pub(super) mod serde_bincode_compat {
    pub use super::deposit::serde_bincode_compat::TxDeposit;
}
