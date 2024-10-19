#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://raw.githubusercontent.com/alloy-rs/core/main/assets/alloy.jpg",
    html_favicon_url = "https://raw.githubusercontent.com/alloy-rs/core/main/assets/favicon.ico"
)]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(any(test, feature = "std")), no_std)]

extern crate alloc;

/// [CHANNEL_ID_LENGTH] is the length of the channel ID.
pub const CHANNEL_ID_LENGTH: usize = 16;

/// [ChannelId] is an opaque identifier for a channel.
pub type ChannelId = [u8; CHANNEL_ID_LENGTH];

mod block;
pub use block::{BlockInfo, FromBlockError, L2BlockInfo};

mod batch_tx;
pub use batch_tx::BatchTransaction;

mod frame;
pub use frame::{
    Frame, FrameDecodingError, FrameParseError, DERIVATION_VERSION_0, FRAME_OVERHEAD, MAX_FRAME_LEN,
};

mod iter;
pub use iter::FrameIter;

mod utils;
pub use utils::{starts_with_2718_deposit, to_system_config, OpBlockConversionError};

mod channel;
pub use channel::{
    Channel, ChannelError, FJORD_MAX_RLP_BYTES_PER_CHANNEL, MAX_RLP_BYTES_PER_CHANNEL,
};

pub mod deposits;
pub use deposits::{
    decode_deposit, DepositError, DepositSourceDomain, DepositSourceDomainIdentifier,
    L1InfoDepositSource, UpgradeDepositSource, UserDepositSource, DEPOSIT_EVENT_ABI_HASH,
};

pub mod block_info;
pub use block_info::{L1BlockInfoBedrock, L1BlockInfoEcotone, L1BlockInfoTx};

pub mod fee;
pub use fee::{
    calculate_tx_l1_cost_bedrock, calculate_tx_l1_cost_ecotone, calculate_tx_l1_cost_fjord,
    calculate_tx_l1_cost_regolith, data_gas_bedrock, data_gas_fjord, data_gas_regolith,
};
