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

mod base;
pub use base::{
    BaseBuiltPayload, BasePayloadAttributes, BasePayloadBuilderAttributes,
    EthPayloadBuilderAttributes,
};

mod base_compat;

mod error;
pub use error::{
    EngineObjectValidationError, InvalidPayloadAttributesError, NewPayloadError,
    PayloadBuilderError, VersionSpecificValidationError,
};

mod traits;
pub use traits::{
    BuildNextEnv, BuiltPayload, BuiltPayloadExecutedBlock, PayloadAttributes,
    PayloadAttributesBuilder, payload_id,
};

mod payload;
pub use payload::{ExecutionPayload, PayloadOrAttributes};

mod validation;
pub use validation::{
    EngineApiMessageVersion, MessageValidationKind, PayloadKind,
    validate_block_access_list_presence, validate_execution_requests,
    validate_parent_beacon_block_root_presence, validate_payload_timestamp,
    validate_slot_number_presence, validate_version_specific_fields, validate_withdrawals_presence,
};

#[cfg(feature = "std")]
mod events;
#[cfg(feature = "std")]
pub use events::{BuiltPayloadStream, Events, PayloadAttributeStream, PayloadEvents};
