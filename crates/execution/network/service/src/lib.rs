#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://raw.githubusercontent.com/paradigmxyz/reth/main/assets/reth-docs.png",
    html_favicon_url = "https://avatars0.githubusercontent.com/u/97369466?s=256",
    issue_tracker_base_url = "https://github.com/paradigmxyz/reth/issues/"
)]
#![allow(unreachable_pub)]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]

#[cfg(any(test, feature = "test-utils"))]
/// Common helpers for network testing.
pub mod test_utils;

pub mod cache;
pub mod config;
pub mod error;
pub mod eth_requests;

pub mod message;
pub mod peers;
pub mod transactions;

mod budget;
mod builder;
mod discovery;
mod fetch;
mod flattened_response;
mod listener;
mod manager;
mod metrics;
mod network;
mod required_block_filter;
mod session;
pub use session::BlockRangeInfo;
mod state;
mod swarm;
mod trusted_peers_resolver;

pub use base_execution_network_wire::{
    DisconnectReason, HelloMessageWithProtocols, NetworkSyncUpdater, PeersConfig, SessionsConfig,
    SyncState,
};
pub use builder::NetworkBuilder;
pub use config::{NetworkConfig, NetworkConfigBuilder};
pub use discovery::Discovery;
pub use fetch::FetchClient;
pub use flattened_response::FlattenedResponse;
pub use manager::NetworkManager;
pub use metrics::TxTypesCounter;
pub use network::NetworkHandle;
pub use session::{
    ActiveSessionHandle, ActiveSessionMessage, EthRlpxConnection, PendingSessionEvent,
    PendingSessionHandle, PendingSessionHandshakeError, SessionCommand, SessionEvent, SessionId,
    SessionManager,
};
pub use swarm::NetworkConnectionState;

/// re-export types crates
pub mod types {
    pub use base_execution_network_discovery::NatResolver;
    pub use base_execution_network_wire::*;
}

use aquamarine as _;
use smallvec as _;

mod api;
pub use api::*;

mod downloads;
pub use downloads::*;
