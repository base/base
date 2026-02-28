#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://avatars0.githubusercontent.com/u/97369466?s=256",
    issue_tracker_base_url = "https://github.com/paradigmxyz/reth/issues/"
)]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(not(feature = "std"), no_std)]

/// The address of the `L2ToL1MessagePasser` predeploy, re-exported from [`base_protocol::Predeploys`].
pub const L2_TO_L1_MESSAGE_PASSER_ADDRESS: alloy_primitives::Address =
    base_protocol::Predeploys::L2_TO_L1_MESSAGE_PASSER;

pub mod transaction;
pub use transaction::*;

mod receipt;

/// Optimism-specific block type.
pub use base_alloy_consensus::OpBlock;
pub use base_alloy_consensus::OpReceipt;
pub use receipt::DepositReceipt;

/// Optimism-specific block body type.
pub type OpBlockBody = <OpBlock as reth_primitives_traits::Block>::Body;

/// Primitive types for Optimism Node.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct OpPrimitives;

impl reth_primitives_traits::NodePrimitives for OpPrimitives {
    type Block = OpBlock;
    type BlockHeader = alloy_consensus::Header;
    type BlockBody = OpBlockBody;
    type SignedTx = OpTransactionSigned;
    type Receipt = OpReceipt;
}

/// Bincode-compatible serde implementations.
#[cfg(feature = "serde-bincode-compat")]
pub mod serde_bincode_compat {
    pub use base_alloy_consensus::serde_bincode_compat::OpReceipt;

    pub use super::receipt::serde_bincode_compat::OpReceipt as LocalOpReceipt;
}
