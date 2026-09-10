#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    issue_tracker_base_url = "https://github.com/base/base/issues/"
)]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(not(feature = "std"), no_std)]

#[macro_use]
extern crate alloc;

mod attributes;
pub use attributes::BasePayloadAttributes;

mod envelope;
pub use envelope::{
    BaseExecutionPayloadEnvelope, ExecutionData, MAX_DECOMPRESSED_ENVELOPE_BYTES,
    NetworkPayloadEnvelope, PayloadEnvelopeEncodeError, PayloadEnvelopeError, PayloadHash,
};

mod sidecar;
pub use sidecar::BaseExecutionPayloadSidecar;

mod payload;
pub use payload::{
    BaseExecutionPayload, BaseExecutionPayloadEnvelopeV3, BaseExecutionPayloadEnvelopeV4,
    BaseExecutionPayloadEnvelopeV5, BaseExecutionPayloadV4, BasePayloadError,
    MAX_TRANSACTIONS_PER_PAYLOAD, MAX_WITHDRAWALS_PER_PAYLOAD,
};
#[cfg(feature = "std")]
pub use payload::{BoundedTransactions, BoundedWithdrawals};

mod cancun;
pub use cancun::*;
mod prague;
pub use prague::*;
mod bogota;
pub use bogota::*;
mod ethereum_sidecar;
pub use ethereum_sidecar::*;
mod forkchoice;
pub use forkchoice::*;
mod ethereum_payload;
pub use ethereum_payload::*;
mod error;
pub use error::*;

#[cfg(feature = "jwt")]
mod jwt;
#[cfg(feature = "jwt")]
pub use jwt::*;

mod builder;
pub use builder::*;

mod engine;
pub use engine::*;
