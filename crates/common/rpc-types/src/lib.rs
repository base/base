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

mod block;
pub use block::{BaseBlockResponse, BaseHeaderResponse};

mod genesis;
pub use genesis::{ChainInfo, FeeInfo, GenesisInfo, UpgradeInfo};

mod log;
pub use log::BaseLogResponse;

mod receipt;
pub use receipt::{
    BaseTransactionReceipt, Eip8130ReceiptFields, L1BlockInfo, TransactionReceiptFields,
};

#[cfg(feature = "eip8130")]
mod eip8130;
#[cfg(feature = "eip8130")]
pub use eip8130::{EIP8130_PRE_ZENITH_RPC_ERROR, Eip8130Nonce};

mod transaction;
pub use transaction::{
    BaseTransactionFields, BaseTransactionRequest, Eip8130AuthScheme, Eip8130RequestFields,
    Transaction,
};

#[cfg(feature = "reth")]
mod reth;
#[cfg(feature = "reth")]
pub use reth::BaseRpcTypes;
