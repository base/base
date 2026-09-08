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
mod engine;
pub use engine::*;
mod error;
pub use error::*;
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

mod core_engine;
pub use core_engine::*;

mod trace;
pub use trace::*;

mod net;
pub use net::*;

#[cfg(any(test, feature = "test-utils"))]
pub mod test_utils;

mod txpool;
pub use txpool::*;

mod validation;
pub use validation::*;

mod reth;
pub use reth::*;

mod core_miner;
pub use core_miner::*;

mod rpc_eth_api;
pub use rpc_eth_api::*;
