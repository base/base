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

mod status;
pub use status::{Status, StatusBuilder, StatusEth69, StatusMessage, UnifiedStatus};

mod version;
pub use version::*;

mod message;
pub use message::*;

mod header;
pub use header::*;

mod blocks;
pub use blocks::*;

mod broadcast;
pub use broadcast::*;

mod transactions;
pub use transactions::*;

mod state;
pub use state::*;

mod receipts;
pub use receipts::*;

mod block_access_lists;
pub use block_access_lists::*;

mod disconnect_reason;
pub use disconnect_reason::*;

mod capability;
pub use capability::*;

mod snap;
/// re-export for convenience
pub use alloy_eips::eip1898::{BlockHashOrNumber, HashOrNumber};
pub use alloy_eips::eip2718::Encodable2718;
pub use snap::*;

#[cfg(feature = "transport")]
mod ecies;
#[cfg(feature = "transport")]
pub use ecies::{
    DEFAULT_BACKPRESSURE_BOUNDARY, ECIES, ECIESCodec, ECIESError, ECIESErrorImpl, ECIESState,
    ECIESStream, EciesCrypto, EgressECIESValue, EncryptedMessage, IngressECIESValue, MAC,
    RLPxSymmetricKeys,
};
