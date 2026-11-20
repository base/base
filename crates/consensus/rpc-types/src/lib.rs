#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    issue_tracker_base_url = "https://github.com/base/base/issues/"
)]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod conductor;
pub use conductor::{ClusterMembership, ServerInfo, ServerSuffrage, UnknownServerSuffrage};

mod health;
pub use health::HealthzResponse;

mod output;
pub use output::OutputResponse;

mod p2p;
pub use p2p::{
    Connectedness, Direction, GossipScores, PeerCount, PeerDump, PeerInfo, PeerScores, PeerStats,
    ReqRespScores, TopicScores,
};

mod safe_head;
pub use safe_head::SafeHeadResponse;

#[cfg(any(feature = "rpc-client", feature = "rpc-server"))]
mod jsonrpsee;
#[cfg(feature = "rpc-client")]
pub use jsonrpsee::{
    AdminApiClient, BaseApiClient, BaseP2PApiClient, ConductorApiClient, RollupNodeApiClient,
};
#[cfg(feature = "rpc-server")]
pub use jsonrpsee::{
    AdminApiServer, BaseApiServer, BaseP2PApiServer, ConductorApiServer, DevEngineApiServer,
    HealthzApiServer, RollupNodeApiServer, WsServer,
};

#[cfg(any(feature = "rpc-client", feature = "rpc-server"))]
mod sync;
#[cfg(feature = "rpc-client")]
pub use sync::SyncStatusApiClient;
#[cfg(feature = "rpc-server")]
pub use sync::SyncStatusApiServer;
