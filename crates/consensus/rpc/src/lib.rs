#![doc = include_str!("../README.md")]
#![doc(issue_tracker_base_url = "https://github.com/base/base/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

#[macro_use]
extern crate tracing;

mod admin;
pub use admin::{AdminRpc, NetworkAdminQuery};

mod client;
pub use client::{EngineRpcClient, SequencerAdminAPIClient, SequencerAdminAPIError};

mod config;
pub use config::RpcBuilder;

mod dev;
pub use dev::DevEngineRpc;

mod health;
pub use health::{HealthzResponse, HealthzRpc};

mod jsonrpsee;
pub use jsonrpsee::{
    AdminApiServer, DevEngineApiServer, HealthzApiServer, MinerApiExtServer, OpAdminApiServer,
    OpP2PApiServer, RollupNodeApiServer, WsServer,
};
#[cfg(feature = "client")]
pub use jsonrpsee::{OpP2PApiClient, RollupNodeApiClient};

mod l1_watcher;
pub use l1_watcher::{L1State, L1WatcherQueries, L1WatcherQuerySender};

mod net;
pub use net::P2pRpc;

mod output;
pub use output::OutputResponse;

mod p2p;

mod response;
pub use response::SafeHeadResponse;

mod rollup;
pub use rollup::RollupRpc;

mod sync;
#[cfg(feature = "client")]
pub use sync::SyncStatusApiClient;
pub use sync::SyncStatusApiServer;

mod ws;
pub use ws::WsRPC;
