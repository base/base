#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    issue_tracker_base_url = "https://github.com/base/base/issues/"
)]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]

extern crate alloc;

mod builder;
pub use builder::{
    BasePayloadBuilder, BasePayloadBuilderCtx, BasePayloadTransactions, Builder, ExecutedPayload,
    ExecutionInfo,
};
mod config;
pub use config::{BaseBuilderConfig, BaseDAConfig, GasLimitConfig};
mod error;
pub use error::BasePayloadBuilderError;
mod payload;
pub use payload::{BaseBuiltPayload, BasePayloadBuilderAttributes, payload_id};
mod traits;
pub use traits::*;
mod types;
pub use types::BasePayloadTypes;
mod validator;
pub use validator::{BaseExecutionPayloadValidator, ensure_well_formed_payload};
