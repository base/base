//! tp2p, an optional gossip transport for unsafe payloads (`base-tiger-p2p`), and the transport
//! that runs it instead of libp2p or alongside it.

mod config;
pub use config::{Tp2pConfig, Tp2pMode, Tp2pModeParseError, Tp2pPeer, Tp2pPeerParseError};

mod transport;
pub use transport::{Tp2pStartError, Tp2pTransport};

mod selected;
pub use selected::{RecentBlockHashes, SelectedGossipTransport};
