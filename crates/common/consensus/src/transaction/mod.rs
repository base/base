//! Transaction types for Base chains.

mod deposit;
pub use deposit::{DepositTransaction, TxDeposit};

mod eip8130;
pub use eip8130::{
    AccountChange, Call, ConfigChange, CreateEntry, Delegation, Eip8130Constants, Eip8130Signed,
    InitialOwner, OwnerChange, OwnerChangeType, Scope, TxEip8130,
};

mod tx_type;
pub use tx_type::{DEPOSIT_TX_TYPE_ID, EIP8130_REJECTION_MSG, EIP8130_TX_TYPE_ID};

mod envelope;
pub use envelope::{BaseTransaction, BaseTxEnvelope, OpTxType};

mod typed;
pub use typed::BaseTypedTransaction;

mod pooled;
#[cfg(feature = "serde")]
pub use deposit::serde_deposit_tx_rpc;
pub use pooled::BasePooledTransaction;

mod meta;
pub use meta::{BaseTransactionInfo, DepositInfo};

/// Bincode-compatible serde implementations for transaction types.
#[cfg(all(feature = "serde", feature = "serde-bincode-compat"))]
pub(super) mod serde_bincode_compat {
    pub use super::{deposit::serde_bincode_compat::TxDeposit, envelope::serde_bincode_compat::*};
}
