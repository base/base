#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    issue_tracker_base_url = "https://github.com/base/base/issues/"
)]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

#[cfg(feature = "server")]
#[macro_use]
extern crate tracing;

#[cfg(feature = "server")]
mod admin;
#[cfg(feature = "server")]
pub use admin::{AdminNetworkAccess, AdminRpc, NetworkAdminQuery, NetworkAdminQuerySender};

#[cfg(feature = "server")]
mod base;
#[cfg(feature = "server")]
pub use base::BaseRpc;

#[cfg(feature = "server")]
mod client;
#[cfg(feature = "server")]
pub use client::{EngineRpcClient, SequencerAdminAPIClient, SequencerAdminAPIError};

#[cfg(feature = "server")]
mod config;
#[cfg(feature = "server")]
pub use config::RpcBuilder;

#[cfg(feature = "server")]
mod dev;
#[cfg(feature = "server")]
pub use dev::DevEngineRpc;

mod health;
pub use health::HealthzResponse;
#[cfg(feature = "server")]
pub use health::HealthzRpc;

mod jsonrpsee;
#[cfg(feature = "client")]
pub use jsonrpsee::{
    AdminApiClient, BaseApiClient, BaseP2PApiClient, ConductorApiClient, RollupNodeApiClient,
};
pub use jsonrpsee::{ClusterMembership, ServerInfo, ServerSuffrage, UnknownServerSuffrage};
#[cfg(feature = "server")]
pub use jsonrpsee::{
    AdminApiServer, BaseApiServer, BaseP2PApiServer, ConductorApiServer, DevEngineApiServer,
    HealthzApiServer, RollupNodeApiServer, WsServer,
};

#[cfg(feature = "server")]
mod l1_watcher;
#[cfg(feature = "server")]
pub use l1_watcher::{L1State, L1WatcherQueries, L1WatcherQuerySender};

#[cfg(feature = "server")]
mod net;
#[cfg(feature = "server")]
pub use net::P2pRpc;

mod output;
pub use output::OutputResponse;

#[cfg(feature = "server")]
mod p2p;

#[cfg(feature = "server")]
mod response;

#[cfg(feature = "server")]
mod rollup;
#[cfg(feature = "server")]
pub use rollup::RollupRpc;

mod sync;
#[cfg(feature = "client")]
pub use sync::SyncStatusApiClient;
#[cfg(feature = "server")]
pub use sync::SyncStatusApiServer;

#[cfg(feature = "server")]
mod ws;
#[cfg(feature = "server")]
pub use ws::WsRPC;
