#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://raw.githubusercontent.com/alloy-rs/core/main/assets/alloy.jpg",
    html_favicon_url = "https://raw.githubusercontent.com/alloy-rs/core/main/assets/favicon.ico"
)]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(not(any(test, feature = "std")), no_std)]

extern crate alloc;

pub use alloy_rpc_types_engine::ForkchoiceUpdateVersion;

mod attributes;
pub use attributes::{OpAttributesWithParent, OpPayloadAttributes};

mod envelope;
pub use envelope::{OpNetworkPayloadEnvelope, PayloadEnvelopeError, PayloadHash};

mod payload_v3;
pub use payload_v3::OpExecutionPayloadEnvelopeV3;

mod payload_v4;
pub use payload_v4::OpExecutionPayloadEnvelopeV4;

mod superchain;
pub use superchain::{
    ProtocolVersion, ProtocolVersionError, ProtocolVersionFormatV0, SuperchainSignal,
};
