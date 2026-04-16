#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    issue_tracker_base_url = "https://github.com/base/base/issues/"
)]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

#[cfg(feature = "reth")]
mod reth_compat;
#[cfg(feature = "reth")]
pub use reth_compat::{BaseBlockBody, BasePrimitives, CompactTxDeposit, DepositReceiptExt};

mod receipts;
pub use receipts::{
    BaseReceipt, BaseReceiptEnvelope, BaseTxReceipt, DepositReceipt, DepositReceiptWithBloom,
};

mod transaction;
#[cfg(feature = "serde")]
pub use transaction::serde_deposit_tx_rpc;
pub use transaction::{
    BasePooledTransaction, BaseTransaction, BaseTransactionInfo, BaseTxEnvelope,
    BaseTypedTransaction, DEPOSIT_TX_TYPE_ID, DepositInfo, DepositTransaction, OpTxType, TxDeposit,
};

mod extra;
pub use extra::{EIP1559ParamError, HoloceneExtraData, JovianExtraData};

mod source;
pub use source::{
    DepositSourceDomain, DepositSourceDomainIdentifier, L1InfoDepositSource, UpgradeDepositSource,
    UserDepositSource,
};

mod predeploys;
pub use predeploys::{Deployers, Predeploys, SystemAddresses};

mod block;
pub use block::BaseBlock;

/// Signed transaction type alias for [`BaseTxEnvelope`].
pub type BaseTransactionSigned = BaseTxEnvelope;

/// Bincode-compatible serde implementations for consensus types.
///
/// `bincode` crate doesn't work well with optionally serializable serde fields, but some of the
/// consensus types require optional serialization for RPC compatibility. This module makes so that
/// all fields are serialized.
///
/// Read more: <https://github.com/bincode-org/bincode/issues/326>
#[cfg(all(feature = "serde", feature = "serde-bincode-compat"))]
pub mod serde_bincode_compat {
    pub use super::{
        receipts::serde_bincode_compat::{BaseReceipt, DepositReceipt},
        transaction::serde_bincode_compat::TxDeposit,
    };

    /// Bincode-compatible serde implementations for transaction types.
    pub mod transaction {
        pub use crate::transaction::serde_bincode_compat::*;
    }
}
