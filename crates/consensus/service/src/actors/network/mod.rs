//! Network Actor

mod actor;
pub use actor::{NetworkActor, NetworkActorError, NetworkInboundData};

mod builder;
pub use builder::NetworkBuilder;

mod config;
pub use config::NetworkConfig;

mod driver;
pub use driver::{NetworkDriver, NetworkDriverError};

mod engine_client;
pub use engine_client::{NetworkEngineClient, QueuedNetworkEngineClient};

mod error;
pub use error::NetworkBuilderError;

mod gossip;
#[cfg(test)]
pub use gossip::MockUnsafePayloadGossipClient;
pub use gossip::{
    QueuedUnsafePayloadGossipClient, UnsafePayloadGossipClient, UnsafePayloadGossipClientError,
};

mod handler;
pub use handler::NetworkHandler;

mod transport;
pub use transport::GossipTransport;

#[cfg(feature = "tp2p")]
mod tp2p;
#[cfg(feature = "tp2p")]
pub use tp2p::{
    RecentBlockHashes, SelectedGossipTransport, Tp2pConfig, Tp2pMode, Tp2pModeParseError, Tp2pPeer,
    Tp2pPeerParseError, Tp2pStartError, Tp2pTransport,
};
