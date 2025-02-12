#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://raw.githubusercontent.com/alloy-rs/core/main/assets/alloy.jpg",
    html_favicon_url = "https://raw.githubusercontent.com/alloy-rs/core/main/assets/favicon.ico"
)]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

pub use alloy_rpc_types_engine::ForkchoiceUpdateVersion;

mod attributes;
pub use attributes::OpPayloadAttributes;

mod envelope;
pub use envelope::{OpExecutionData, OpNetworkPayloadEnvelope, PayloadEnvelopeError, PayloadHash};

mod sidecar;
pub use sidecar::OpExecutionPayloadSidecar;

pub mod payload;
pub use payload::{
    v3::OpExecutionPayloadEnvelopeV3,
    v4::{OpExecutionPayloadEnvelopeV4, OpExecutionPayloadV4},
    OpExecutionPayload,
};
