#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://raw.githubusercontent.com/alloy-rs/core/main/assets/alloy.jpg",
    html_favicon_url = "https://raw.githubusercontent.com/alloy-rs/core/main/assets/favicon.ico"
)]
#![warn(
    missing_copy_implementations,
    missing_debug_implementations,
    missing_docs,
    unreachable_pub,
    clippy::missing_const_for_fn,
    rustdoc::all
)]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![deny(unused_must_use, rust_2018_idioms)]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(feature = "std"), no_std)]

#[cfg(not(feature = "std"))]
extern crate alloc;

/// [CHANNEL_ID_LENGTH] is the length of the channel ID.
pub const CHANNEL_ID_LENGTH: usize = 16;

/// [ChannelId] is an opaque identifier for a channel.
pub type ChannelId = [u8; CHANNEL_ID_LENGTH];

mod block;
pub use block::{BlockInfo, L2BlockInfo};

mod batch_tx;
pub use batch_tx::BatchTransaction;

mod frame;
pub use frame::{Frame, FrameDecodingError, FrameParseError};

mod channel;
pub use channel::{Channel, FJORD_MAX_RLP_BYTES_PER_CHANNEL, MAX_RLP_BYTES_PER_CHANNEL};
