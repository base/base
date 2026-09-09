//! Implementation of the `eth` wire protocol.
//!
//! ## Feature Flags
//!
//! - `serde` (default): Enable serde support
//! - `arbitrary`: Adds `proptest` and `arbitrary` support for wire types.

#![doc(
    html_logo_url = "https://raw.githubusercontent.com/paradigmxyz/reth/main/assets/reth-docs.png",
    html_favicon_url = "https://avatars0.githubusercontent.com/u/97369466?s=256",
    issue_tracker_base_url = "https://github.com/paradigmxyz/reth/issues/"
)]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]

pub mod capability;
mod disconnect;
pub mod errors;
pub mod eth_snap;
mod ethstream;
mod hello;
mod p2pstream;
mod pinger;
pub mod protocol;

/// Handshake logic
pub mod handshake;

#[cfg(test)]
pub mod test_utils;

// Re-export wire types
#[doc(inline)]
pub use base_execution_network_wire::*;
#[cfg(test)]
pub use tokio_util::codec::{
    LengthDelimitedCodec as PassthroughCodec, LengthDelimitedCodecError as PassthroughCodecError,
};

pub use crate::Capability;
pub use crate::ProtocolVersion;
pub use crate::disconnect::CanDisconnect;
pub use crate::eth_snap::EthSnapMessage;
pub use crate::eth_snap::EthSnapStream;
pub use crate::ethstream::EthStream;
pub use crate::ethstream::EthStreamInner;
pub use crate::ethstream::UnauthedEthStream;
pub use crate::hello::HelloMessage;
pub use crate::hello::HelloMessageBuilder;
pub use crate::hello::HelloMessageWithProtocols;
pub use crate::p2pstream::DisconnectP2P;
pub use crate::p2pstream::HANDSHAKE_TIMEOUT;
pub use crate::p2pstream::MAX_RESERVED_MESSAGE_ID;
pub use crate::p2pstream::P2PMessage;
pub use crate::p2pstream::P2PMessageID;
pub use crate::p2pstream::P2PStream;
pub use crate::p2pstream::UnauthedP2PStream;
