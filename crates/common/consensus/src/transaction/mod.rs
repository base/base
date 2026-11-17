//! Transaction types for Base chains.

mod canonical;
pub use canonical::decode_2718_canonical;

mod deposit;
pub use deposit::{DepositTransaction, TxDeposit};

mod base_tx;
#[cfg(feature = "serde")]
pub use base_tx::TxBasePayerSerde;
pub use base_tx::{
    AccountChange, Call, CoinbaseTip, Delegation, TxBase, TxBaseConstants, TxBaseSigned,
    TxBaseStaticError, TxBaseStructuralError, TxBaseStructure, TxBaseTimestampError,
};

mod tx_type;
pub use tx_type::{BASE_TX_REJECTION_MSG, BASE_TX_TYPE_ID, DEPOSIT_TX_TYPE_ID};

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
