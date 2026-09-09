//! Reth RPC interface definitions
//!
//! Provides all RPC interfaces.
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

mod admin;
mod debug;
mod mev;
mod miner;
mod net;
mod otterscan;
mod reth;
mod rpc;
mod trace;
mod txpool;
mod web3;

pub use reth::RethJitAction;
/// re-export of all server traits
pub use servers::*;

/// Aggregates all server traits.
pub mod servers {

    pub use crate::{
        admin::AdminApiServer, debug::DebugApiServer, mev::MevSimApiServer, miner::MinerApiServer,
        net::NetApiServer, otterscan::OtterscanServer, reth::RethApiServer, rpc::RpcApiServer,
        trace::TraceApiServer, txpool::TxPoolApiServer, web3::Web3ApiServer,
    };
}

/// re-export of all client traits
#[cfg(feature = "client")]
pub use clients::*;

/// Aggregates all client traits.
#[cfg(feature = "client")]
pub mod clients {

    pub use crate::{
        admin::AdminApiClient, debug::DebugApiClient, mev::MevSimApiClient, miner::MinerApiClient,
        net::NetApiClient, otterscan::OtterscanClient, reth::RethApiClient, rpc::RpcApiClient,
        trace::TraceApiClient, txpool::TxPoolApiClient, web3::Web3ApiClient,
    };
}
