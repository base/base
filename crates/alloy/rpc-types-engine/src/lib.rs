#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://raw.githubusercontent.com/alloy-rs/core/main/assets/alloy.jpg",
    html_favicon_url = "https://raw.githubusercontent.com/alloy-rs/core/main/assets/favicon.ico"
)]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

mod attributes;
pub use attributes::{OptimismAttributesWithParent, OptimismPayloadAttributes};

mod payload_v3;
pub use payload_v3::OptimismExecutionPayloadEnvelopeV3;

mod payload_v4;
pub use payload_v4::OptimismExecutionPayloadEnvelopeV4;

mod superchain;
pub use superchain::{
    ProtocolVersion, ProtocolVersionError, ProtocolVersionFormatV0, SuperchainSignal,
};
