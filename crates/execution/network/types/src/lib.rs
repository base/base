#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://raw.githubusercontent.com/paradigmxyz/reth/main/assets/reth-docs.png",
    html_favicon_url = "https://avatars0.githubusercontent.com/u/97369466?s=256",
    issue_tracker_base_url = "https://github.com/paradigmxyz/reth/issues/"
)]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

mod identity;
pub use identity::{PeerId, AnyNode, WithPeerId};
#[cfg(feature = "secp256k1")]
pub use identity::{pk2id, id2pk};
#[cfg(feature = "secp256k1")]
pub use enr::Enr;
mod node_record;
pub use node_record::{NodeRecord, NodeRecordParseError};
mod trusted_peer;
pub use trusted_peer::TrustedPeer;
mod bootnodes;
pub use bootnodes::*;
