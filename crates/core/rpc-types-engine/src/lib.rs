#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    issue_tracker_base_url = "https://github.com/base/base/issues/"
)]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

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
    BlobsBundleV2, OpExecutionPayload, OpExecutionPayloadEnvelopeV3, OpExecutionPayloadEnvelopeV4,
    OpExecutionPayloadEnvelopeV5, OpExecutionPayloadV4, OpPayloadError,
};

#[cfg(feature = "reth")]
mod reth;
