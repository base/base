#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://raw.githubusercontent.com/alloy-rs/core/main/assets/favicon.ico"
)]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

pub use alloy_rpc_types_engine::ForkchoiceUpdateVersion;

mod attributes;
pub use attributes::OpPayloadAttributes;

mod envelope;
pub use envelope::{
    OpExecutionData, OpExecutionPayloadEnvelope, OpNetworkPayloadEnvelope,
    PayloadEnvelopeEncodeError, PayloadEnvelopeError, PayloadHash,
};

mod sidecar;
pub use sidecar::OpExecutionPayloadSidecar;

mod payload;
pub use payload::{
    OpExecutionPayload, OpExecutionPayloadEnvelopeV3, OpExecutionPayloadEnvelopeV4,
    OpExecutionPayloadV4, OpPayloadError,
};

mod flashblock;
pub use flashblock::{
    OpFlashblockError, OpFlashblockPayload, OpFlashblockPayloadBase, OpFlashblockPayloadDelta,
    OpFlashblockPayloadMetadata,
};

#[cfg(feature = "reth")]
mod reth;
