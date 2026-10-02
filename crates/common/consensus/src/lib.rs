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
    Eip8130Receipt,
};

mod transaction;
pub use transaction::{
    AccountChange, AccountChangeChannel, BasePooledTransaction, BaseTransaction,
    BaseTransactionInfo, BaseTxEnvelope, BaseTypedTransaction, Call, ChangeType, CoinbaseTip,
    CreateEntry, DEPOSIT_TX_TYPE_ID, Delegation, DepositInfo, DepositTransaction,
    EIP8130_REJECTION_MSG, EIP8130_TX_TYPE_ID, Eip8130Constants, Eip8130Contracts, Eip8130Signed,
    Eip8130StaticError, Eip8130TimestampError, IDefaultAccount, InitialActor, OpTxType, Scope,
    SignedAccountChanges, SignedChange, TxDeposit, TxEip8130, decode_2718_canonical,
};
#[cfg(feature = "serde")]
pub use transaction::{Eip8130PayerSerde, serde_deposit_tx_rpc};

mod extra;
pub use extra::{EIP1559ParamEncoder, EIP1559ParamError, HoloceneExtraData, JovianExtraData};

mod source;
pub use source::{
    BaseTimeDepositSource, DepositSourceDomain, DepositSourceDomainIdentifier, L1InfoDepositSource,
    UpgradeDepositSource, UserDepositSource,
};

mod base_time;
pub use base_time::{
    BaseTimeMetadataError, BaseTimeScheduleError, BaseTimeUpdateDecodeError, BaseTimeUpdateError,
    BaseTimeUpdateTx,
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
pub(crate) mod test_utils {
    use alloy_primitives::hex;

    use crate::{L1BlockInfoBedrock, L1BlockInfoEcotone, L1BlockInfoIsthmus};

    pub(crate) const RAW_BEDROCK_INFO_TX: [u8; L1BlockInfoBedrock::L1_INFO_TX_LEN] = hex!(
        "015d8eb9000000000000000000000000000000000000000000000000000000000117c4eb0000000000000000000000000000000000000000000000000000000065280377000000000000000000000000000000000000000000000000000000026d05d953392012032675be9f94aae5ab442de73c5f4fb1bf30fa7dd0d2442239899a40fc00000000000000000000000000000000000000000000000000000000000000040000000000000000000000006887246668a3b87f54deb3b94ba47a6f63f3298500000000000000000000000000000000000000000000000000000000000000bc00000000000000000000000000000000000000000000000000000000000a6fe0"
    );
    pub(crate) const RAW_ECOTONE_INFO_TX: [u8; L1BlockInfoEcotone::L1_INFO_TX_LEN] = hex!(
        "440a5e2000000558000c5fc5000000000000000500000000661c277300000000012bec20000000000000000000000000000000000000000000000000000000026e9f109900000000000000000000000000000000000000000000000000000000000000011c4c84c50740386c7dc081efddd644405f04cde73e30a2e381737acce9f5add30000000000000000000000006887246668a3b87f54deb3b94ba47a6f63f32985"
    );
    pub(crate) const RAW_ISTHMUS_INFO_TX: [u8; L1BlockInfoIsthmus::L1_INFO_TX_LEN] = hex!(
        "098999be00000558000c5fc5000000000000000500000000661c277300000000012bec20000000000000000000000000000000000000000000000000000000026e9f109900000000000000000000000000000000000000000000000000000000000000011c4c84c50740386c7dc081efddd644405f04cde73e30a2e381737acce9f5add30000000000000000000000006887246668a3b87f54deb3b94ba47a6f63f329850000abcd000000000000dcba"
    );
}

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
