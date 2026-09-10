#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    issue_tracker_base_url = "https://github.com/base/base/issues/"
)]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]

mod config;

pub use config::*;
mod debug;
pub use debug::*;
mod eth;
pub use eth::*;
mod metrics;
pub use metrics::*;
mod miner;
pub use miner::*;
mod sequencer;
pub use sequencer::*;
mod state;
pub use state::*;
mod trace_middleware;
pub use trace_middleware::{
    InboundOtelContext, OtelHttpMiddleware, OtelHttpMiddlewareLayer, OtelRpcMiddleware,
    OtelRpcMiddlewareLayer,
};
mod witness;
pub use witness::*;

mod rpc;
pub use rpc::*;

mod core_debug;
pub use core_debug::*;

mod otterscan;
pub use otterscan::*;

mod admin;
pub use admin::*;

mod web3;
pub use web3::*;

mod eth_backend;
pub use eth_backend::*;

mod trace;
pub use trace::*;

mod net;
pub use net::*;

#[cfg(any(test, feature = "test-utils"))]
pub mod test_utils;

mod txpool;
pub use txpool::*;

mod reth;
pub use reth::*;

mod core_miner;
pub use core_miner::*;

mod rpc_eth_api;
pub use rpc_eth_api::*;

mod rpc_api_rpc;
#[cfg(feature = "client")]
pub use rpc_api_rpc::RpcApiClient;
pub use rpc_api_rpc::RpcApiServer;

mod rpc_api_debug;
#[cfg(feature = "client")]
pub use rpc_api_debug::DebugApiClient;
pub use rpc_api_debug::DebugApiServer;

mod rpc_api_otterscan;
#[cfg(feature = "client")]
pub use rpc_api_otterscan::OtterscanClient;
pub use rpc_api_otterscan::OtterscanServer;

mod rpc_api_admin;
#[cfg(feature = "client")]
pub use rpc_api_admin::AdminApiClient;
pub use rpc_api_admin::AdminApiServer;

mod rpc_api_web3;
#[cfg(feature = "client")]
pub use rpc_api_web3::Web3ApiClient;
pub use rpc_api_web3::Web3ApiServer;

mod rpc_api_trace;
#[cfg(feature = "client")]
pub use rpc_api_trace::TraceApiClient;
pub use rpc_api_trace::TraceApiServer;

mod rpc_api_net;
#[cfg(feature = "client")]
pub use rpc_api_net::NetApiClient;
pub use rpc_api_net::NetApiServer;

mod rpc_api_txpool;
#[cfg(feature = "client")]
pub use rpc_api_txpool::TxPoolApiClient;
pub use rpc_api_txpool::TxPoolApiServer;

mod rpc_api_mev;
#[cfg(feature = "client")]
pub use rpc_api_mev::MevSimApiClient;
pub use rpc_api_mev::MevSimApiServer;

mod rpc_api_reth;
#[cfg(feature = "client")]
pub use rpc_api_reth::RethApiClient;
pub use rpc_api_reth::RethApiServer;

pub use rpc_api_reth::RethJitAction;
mod rpc_api_miner;
#[cfg(feature = "client")]
pub use rpc_api_miner::MinerApiClient;
pub use rpc_api_miner::MinerApiServer;

mod eth_services;
pub use eth_services::*;

extern crate alloc;

mod conversion;
pub use conversion::*;

mod eip8130;
pub use eip8130::*;
