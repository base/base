//! P2P subprotocol for encrypted transaction relay.
//!
//! This module implements a custom devp2p subprotocol for forwarding encrypted
//! transactions from relay nodes to the sequencer.
//!
//! # Components
//!
//! - [`messages`]: RLP-encoded wire protocol messages
//! - [`discovery`]: ENR-based sequencer discovery using attestations
//! - [`forwarder`]: Non-sequencer forwarding task
//! - [`sequencer`]: Sequencer processing task
//! - [`handler`]: RLPx protocol handler (requires `p2p` feature)
//! - [`network`]: Network implementation of peer sender (requires `p2p` feature)

pub mod discovery;
pub mod forwarder;
pub mod messages;
pub mod sequencer;

#[cfg(feature = "p2p")]
pub mod handler;
#[cfg(feature = "p2p")]
pub mod network;

pub use discovery::{PeerId, SequencerPeerTracker, encode_enr_attestation, decode_enr_attestation, ENR_ATTESTATION_KEY};
pub use forwarder::{ForwardRequest, ForwardResult, RelayForwarder, create_forward_channel};
pub use messages::{AckMessage, EncryptedTxMessage, PROTOCOL_NAME, PROTOCOL_VERSION};
pub use sequencer::{SequencerProcessor, TransactionSubmitter, create_receive_channel};

#[cfg(feature = "p2p")]
pub use handler::{
    ConnectionMessage, EncryptedRelayConnectionHandler, EncryptedRelayProtoHandler,
    ProtocolEvent, ProtocolState,
};
#[cfg(feature = "p2p")]
pub use network::{NetworkPeerSender, ProtocolEventHandler, SequencerEventHandler};
