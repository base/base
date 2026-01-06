#![doc = include_str!("../README.md")]
#![doc(issue_tracker_base_url = "https://github.com/base/node-reth/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod block;
pub use block::Flashblock;

mod metadata;
pub use metadata::Metadata;

mod error;
pub use error::FlashblockDecodeError;

mod payload;
pub use payload::{
    ExecutionPayloadBaseV1, ExecutionPayloadFlashblockDeltaV1, FlashblocksPayloadV1,
};

pub mod validation;
pub use validation::{
    is_non_zero_address, is_valid_blob_gas, is_valid_block_hash, is_valid_gas_usage,
    is_valid_state_root, is_valid_timestamp, is_valid_transaction_bytes,
};
