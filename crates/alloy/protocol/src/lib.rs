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
pub use frame::{
    Frame, FrameDecodingError, FrameParseError, DERIVATION_VERSION_0, FRAME_OVERHEAD, MAX_FRAME_LEN,
};

mod utils;
pub use utils::starts_with_2718_deposit;

mod channel;
pub use channel::{Channel, FJORD_MAX_RLP_BYTES_PER_CHANNEL, MAX_RLP_BYTES_PER_CHANNEL};

pub mod deposits;
pub use deposits::{
    decode_deposit, DepositError, DepositSourceDomain, DepositSourceDomainIdentifier,
    L1InfoDepositSource, UpgradeDepositSource, UserDepositSource, DEPOSIT_EVENT_ABI_HASH,
};

pub mod block_info;
pub use block_info::{L1BlockInfoBedrock, L1BlockInfoEcotone, L1BlockInfoTx};
