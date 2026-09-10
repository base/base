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
extern crate self as base_common_types_chain;

#[cfg(feature = "reth")]
mod reth_compat;
#[cfg(feature = "reth")]
pub use reth_compat::CompactTxDeposit;

mod receipts;
pub use receipts::{
    BaseReceipt, BaseReceiptEnvelope, BaseTxReceipt, DepositReceipt, DepositReceiptWithBloom,
    Eip8130Receipt,
};

mod base_transaction;
#[cfg(feature = "serde")]
pub use base_transaction::serde_deposit_tx_rpc;
pub use base_transaction::{
    AccountChange, AccountChangeChannel, BasePooledTransaction, BaseTransaction,
    BaseTransactionInfo, BaseTxEnvelope, BaseTypedTransaction, Call, ChangeType, CoinbaseTip,
    CreateEntry, DEPOSIT_TX_TYPE_ID, Delegation, DepositInfo, DepositTransaction,
    EIP8130_REJECTION_MSG, EIP8130_TX_TYPE_ID, Eip8130Constants, Eip8130Contracts, Eip8130Signed,
    Eip8130StaticError, Eip8130TimestampError, IDefaultAccount, InitialActor, OpTxType, Scope,
    SignedAccountChanges, SignedChange, TxDeposit, TxEip8130,
};

mod extra;
pub use extra::{EIP1559ParamEncoder, EIP1559ParamError, HoloceneExtraData, JovianExtraData};

mod source;
pub use source::{
    BaseTimeDepositSource, DepositSourceDomain, DepositSourceDomainIdentifier, L1InfoDepositSource,
    UpgradeDepositSource, UserDepositSource,
};

mod predeploys;
pub use predeploys::{Deployers, Predeploys, SystemAddresses};

mod base_block;
pub use base_block::{BaseBlock, BaseBlockBody};

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
        base_transaction::serde_bincode_compat::TxDeposit,
        receipts::serde_bincode_compat::{BaseReceipt, DepositReceipt},
    };
    pub use crate::{
        block::serde_bincode_compat::*, receipt::serde_bincode_compat::*,
        transaction::serde_bincode_compat::*,
    };

    /// Bincode-compatible serde implementations for transaction types.
    pub mod transaction {
        pub use crate::{
            base_transaction::serde_bincode_compat::*, transaction::serde_bincode_compat::*,
        };
    }
}

mod chain_info;
pub use alloy_trie::TrieAccount;
pub use chain_info::ChainInfo;
use once_cell as _;
#[cfg(feature = "arbitrary")]
use rand_08 as _;

/// Represents an TrieAccount in the account trie
#[deprecated(since = "0.7.3", note = "use TrieAccount instead")]
pub type Account = TrieAccount;

mod block;
pub use block::{
    Block, BlockBody, BlockHeader, EthBlock, GasLimitMismatch, Header, HeaderInfo, HeaderRoots,
};

mod indexed;
pub use indexed::Indexed;

pub mod constants;
pub use constants::{EMPTY_OMMER_ROOT_HASH, EMPTY_ROOT_HASH};

mod receipt;
pub use receipt::{
    Eip658Value, Eip2718DecodableReceipt, Eip2718EncodableReceipt, EthereumReceipt, Receipt,
    ReceiptEnvelope, ReceiptWithBloom, Receipts, RlpDecodableReceipt, RlpEncodableReceipt,
    TxReceipt, TxTy,
};

pub mod size;
pub use size::InMemorySize;

pub mod conditional;
pub mod proofs;

pub mod transaction;
#[cfg(feature = "kzg")]
pub use alloy_eips::eip4844::env_settings::EnvKzgSettings;
pub use alloy_eips::{
    Typed2718,
    eip4844::{
        Blob, BlobTransactionSidecar, Bytes48,
        builder::{SidecarBuilder, SidecarCoder, SimpleCoder},
        utils,
    },
    eip7594::{BlobTransactionSidecarEip7594, BlobTransactionSidecarVariant},
};
pub use alloy_primitives::{Sealable, Sealed};
#[cfg(feature = "kzg")]
pub use transaction::BlobTransactionValidationError;
pub use transaction::{
    EthereumTxEnvelope, EthereumTypedTransaction, SignableTransaction, Transaction,
    TransactionEnvelope, TxEip1559, TxEip2930, TxEip4844, TxEip4844Variant, TxEip4844WithSidecar,
    TxEip7702, TxEnvelope, TxLegacy, TxType, TypedTransaction, decode_2718_canonical,
};

mod signed;
pub use alloy_tx_macros::TransactionEnvelope;
pub use signed::Signed;

pub mod crypto;
pub mod error;

pub mod extended;
pub use extended::Extended;

#[doc(hidden)]
pub mod private {
    pub use alloy_eips;
    pub use alloy_primitives;
    pub use alloy_rlp;
    #[cfg(feature = "serde")]
    pub use alloy_serde;
    pub use alloy_trie;
    #[cfg(feature = "arbitrary")]
    pub use arbitrary;
    #[cfg(feature = "serde")]
    pub use serde;
    #[cfg(feature = "serde")]
    pub use serde_json;
}

mod compact;
pub use base_common_codec_macros::*;
pub use compact::{Compact, CompactPlaceholder};

#[cfg(feature = "alloy")]
pub mod alloy;
#[cfg(feature = "alloy")]
pub use alloy::ReceiptFlags;

pub mod compress;
pub use compress::{Compress, Decompress, DecompressError};

pub mod txtype;

#[cfg(any(test, feature = "test-utils"))]
pub mod test_utils;

#[doc(hidden)]
#[path = "compact_private.rs"]
pub mod __private;

mod sealed_header;
pub use sealed_header::SealedHeader;

mod output_root;
pub use output_root::OutputRoot;

mod block_info;
pub use block_info::{BlockInfo, L2BlockInfo};

#[cfg(feature = "k256")]
mod sealed_block;
pub use alloy_eips::eip2718::WithEncoded;
pub use alloy_primitives::{Log, LogData, logs_bloom};
pub use crypto::RecoveryError;
#[cfg(feature = "k256")]
pub use sealed_block::{BlockRecoveryError, SealedBlockRecoveryError};
#[cfg(all(feature = "k256", feature = "dashmap"))]
pub use sealed_block::{DashMap, DashSet, Entry, mapref};
#[cfg(all(feature = "k256", any(test, feature = "arbitrary", feature = "test-utils")))]
pub mod primitive_header_test_utils;
#[cfg(feature = "k256")]
pub use sealed_block::gas_spent_by_transactions;
#[cfg(feature = "k256")]
pub use sealed_block::{
    BlockBody as BlockBodyExt, BlockHeader as BlockHeaderExt, EthereumReceiptRoot, GotExpected,
    GotExpectedBoxed, IndexedTx, MaybeSerde, RecoveredBlock, SealedBlock, SealedBlockWith,
    SealedOrRecoveredBlock, SignedTransaction,
};
#[cfg(feature = "k256")]
pub use sealed_block::{
    InvalidTransactionError, TransactionConversionError, TryFromRecoveredTransactionError,
};
#[cfg(feature = "k256")]
pub use sealed_block::{LazyLock, OnceLock};
#[cfg(feature = "k256")]
pub use sealed_block::{recover_signers, recover_signers_unchecked, try_recover_signers};
pub use transaction::{Recovered, SignerRecoverable, TransactionInfo, TransactionMeta, TxHashRef};
