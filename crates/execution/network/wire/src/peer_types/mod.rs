//! Network peer types and utils.

mod identity;
#[cfg(feature = "secp256k1")]
pub use enr::Enr;
pub use identity::{AnyNode, PeerId, WithPeerId};
#[cfg(feature = "secp256k1")]
pub use identity::{id2pk, pk2id};
mod node_record;
pub use node_record::{NodeRecord, NodeRecordParseError};
mod trusted_peer;
pub use trusted_peer::TrustedPeer;
mod bootnodes;
pub use bootnodes::*;

#[cfg(feature = "std")]
mod banlist;
#[cfg(feature = "std")]
pub use banlist::{BanList, IpFilter};

#[cfg(feature = "std")]
mod backoff;
#[cfg(feature = "std")]
pub use backoff::BackoffKind;

#[cfg(feature = "std")]
mod peer;
#[cfg(feature = "std")]
pub use peer::{Peer, PersistedPeerInfo};

#[cfg(feature = "std")]
mod peer_addr;
#[cfg(feature = "std")]
pub use peer_addr::PeerAddr;

#[cfg(feature = "std")]
mod peer_config;
#[cfg(feature = "std")]
pub use peer_config::{
    ConnectionsConfig, DEFAULT_MAX_COUNT_CONCURRENT_OUTBOUND_DIALS,
    DEFAULT_MAX_COUNT_PEERS_INBOUND, DEFAULT_MAX_COUNT_PEERS_OUTBOUND,
    DEFAULT_PEER_ROTATION_INTERVAL, INBOUND_IP_THROTTLE_DURATION, PEER_ROTATION_MIN_UPTIME,
    PeerBackoffDurations, PeersConfig,
};

#[cfg(feature = "std")]
mod peer_kind;
#[cfg(feature = "std")]
pub use peer_kind::PeerKind;

#[cfg(feature = "std")]
mod reputation;
#[cfg(feature = "std")]
pub use reputation::{
    BANNED_REPUTATION, DEFAULT_REPUTATION, FAILED_TO_CONNECT_REPUTATION_CHANGE,
    MAX_TRUSTED_PEER_REPUTATION_CHANGE, Reputation, ReputationChange, ReputationChangeKind,
    ReputationChangeOutcome, ReputationChangeWeights, is_banned_reputation,
    is_connection_failed_reputation,
};

#[cfg(feature = "std")]
mod peer_state;
#[cfg(feature = "std")]
pub use peer_state::PeerConnectionState;

#[cfg(feature = "std")]
mod session_config;
#[cfg(feature = "std")]
pub use session_config::{
    INITIAL_REQUEST_TIMEOUT, PENDING_SESSION_TIMEOUT, PROTOCOL_BREACH_REQUEST_TIMEOUT,
    SessionLimits, SessionsConfig,
};
