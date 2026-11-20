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

/// The system transaction gas limit post-Regolith.
pub const REGOLITH_SYSTEM_TX_GAS: u64 = 1_000_000;

#[cfg(feature = "evm")]
use revm as _;

#[cfg(feature = "reth")]
mod reth_compat;
#[cfg(feature = "reth")]
pub use reth_compat::{BaseBlockBody, BasePrimitives, CompactTxDeposit, DepositReceiptExt};

mod receipts;
pub use receipts::{
    BaseReceipt, BaseReceiptEnvelope, BaseTxReceipt, DepositReceipt, DepositReceiptWithBloom,
    TxBaseCommittedPhasesError, TxBaseReceipt,
};

mod transaction;
pub use transaction::{
    AccountChange, BASE_TX_REJECTION_MSG, BASE_TX_TYPE_ID, BasePooledTransaction, BaseTransaction,
    BaseTransactionInfo, BaseTxEnvelope, BaseTypedTransaction, Call, CoinbaseTip,
    DEPOSIT_TX_TYPE_ID, Delegation, DepositInfo, DepositTransaction, OpTxType, TxBase,
    TxBaseConstants, TxBaseSigned, TxBaseStaticError, TxBaseStructuralError, TxBaseStructure,
    TxBaseTimestampError, TxDeposit, decode_2718_canonical,
};
#[cfg(feature = "serde")]
pub use transaction::{TxBasePayerSerde, serde_deposit_tx_rpc};

mod extra;
pub use extra::{EIP1559ParamEncoder, EIP1559ParamError, HoloceneExtraData, JovianExtraData};

mod source;
pub use source::{
    BaseTimeDepositSource, DepositSourceDomain, DepositSourceDomainIdentifier, L1InfoDepositSource,
    UpgradeDepositSource, UserDepositSource,
};

mod base_time;
pub use base_time::{
    BaseTimeMetadataError, BaseTimePredeploy, BaseTimeScheduleError, BaseTimeUpdateDecodeError,
    BaseTimeUpdateError, BaseTimeUpdateTx,
};

mod info;
pub use info::{
    BlockInfoError, DecodeError, L1BlockInfoBedrock, L1BlockInfoBedrockBase,
    L1BlockInfoBedrockBaseFields, L1BlockInfoBedrockFields, L1BlockInfoBedrockOnlyFields,
    L1BlockInfoEcotone, L1BlockInfoEcotoneBase, L1BlockInfoEcotoneBaseFields,
    L1BlockInfoEcotoneFields, L1BlockInfoEcotoneOnlyFields, L1BlockInfoIsthmus,
    L1BlockInfoIsthmusBaseFields, L1BlockInfoIsthmusFields, L1BlockInfoJovian,
    L1BlockInfoJovianBaseFields, L1BlockInfoJovianFields, L1BlockInfoTx,
};

#[cfg(test)]
pub(crate) mod test_utils;

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
