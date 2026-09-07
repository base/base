//! Reth RPC `eth_` API implementation
//!
//! ## Feature Flags
//!
//! - `client`: Enables JSON-RPC client support.

#![doc(
    html_logo_url = "https://raw.githubusercontent.com/paradigmxyz/reth/main/assets/reth-docs.png",
    html_favicon_url = "https://avatars0.githubusercontent.com/u/97369466?s=256",
    issue_tracker_base_url = "https://github.com/paradigmxyz/reth/issues/"
)]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]

pub mod bundle;
pub mod core;
pub mod ext;
pub mod filter;
pub mod helpers;
pub mod node;
pub mod pubsub;
pub mod types;

#[cfg(feature = "client")]
pub use core::EthApiClient;
pub use core::{EthApiServer, FullEthApiServer};

#[cfg(feature = "client")]
pub use bundle::{EthBundleApiClient, EthCallBundleApiClient};
pub use bundle::{EthBundleApiServer, EthCallBundleApiServer};
#[cfg(feature = "client")]
pub use ext::L2EthApiExtClient;
pub use ext::L2EthApiExtServer;
#[cfg(feature = "client")]
pub use filter::EthFilterApiClient;
pub use filter::{EngineEthFilter, EthFilterApiServer, QueryLimits};
#[cfg(feature = "client")]
pub use helpers::config::EthConfigApiClient;
pub use helpers::config::EthConfigApiServer;
pub use node::{RpcNodeCore, RpcNodeCoreExt};
pub use pubsub::EthPubSubApiServer;
pub use reth_rpc_convert::*;
pub use reth_rpc_eth_types::error::{
    AsEthApiError, FromEthApiError, FromEvmError, IntoEthApiError,
};
use reth_trie_common as _;
pub use types::{EthApiTypes, FullEthApiTypes};

mod base_receipt;
pub use base_receipt::{BaseReceiptBuilder, BaseReceiptConverter, ReceiptFieldsBuilder};
mod base_time;
pub use base_time::BaseTimeCache;
mod base_tx_info;
pub use base_tx_info::BaseTxInfoMapper;

mod base_rpc_converter;
pub use base_rpc_converter::BaseRpcConverter;
