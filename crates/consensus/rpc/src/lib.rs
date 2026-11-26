#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    issue_tracker_base_url = "https://github.com/base/base/issues/"
)]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

#[macro_use]
extern crate tracing;

#[cfg(feature = "client")]
pub use base_consensus_rpc_types::{
    AdminApiClient, BaseApiClient, BaseP2PApiClient, ConductorApiClient, RollupNodeApiClient,
    SyncStatusApiClient,
};
pub use base_consensus_rpc_types::{
    AdminApiServer, BaseApiServer, BaseP2PApiServer, ClusterMembership, ConductorApiServer,
    Connectedness, DevEngineApiServer, Direction, GossipScores, HealthzApiServer, HealthzResponse,
    OutputResponse, PeerCount, PeerDump, PeerInfo, PeerScores, PeerStats, ReqRespScores,
    RollupNodeApiServer, SafeHeadResponse, ServerInfo, ServerSuffrage, SyncStatusApiServer,
    TopicScores, UnknownServerSuffrage, WsServer,
};

mod admin;
pub use admin::{AdminNetworkAccess, AdminRpc, NetworkAdminQuery, NetworkAdminQuerySender};

mod base;
pub use base::BaseRpc;

mod client;
pub use client::{EngineRpcClient, SequencerAdminAPIClient, SequencerAdminAPIError};

mod config;
pub use config::RpcBuilder;

mod dev;
pub use dev::DevEngineRpc;

mod health;
pub use health::HealthzRpc;

mod l1_watcher;
pub use l1_watcher::{L1State, L1WatcherQueries, L1WatcherQuerySender};

mod net;
pub use net::P2pRpc;

mod p2p;

mod response;

mod rollup;
pub use rollup::RollupRpc;

mod ws;
pub use ws::WsRPC;
