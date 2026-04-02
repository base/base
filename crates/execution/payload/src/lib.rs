#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    issue_tracker_base_url = "https://github.com/base/base/issues/"
)]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![allow(clippy::useless_let_if_seq)]

extern crate alloc;

pub mod builder;
pub use builder::OpPayloadBuilder;
pub mod config;
pub mod error;
pub mod payload;
pub use payload::{
    OpBuiltPayload, OpPayloadAttributes, OpPayloadBuilderAttributes, payload_id_optimism,
};
mod traits;
pub use traits::*;
mod types;
pub use types::OpPayloadTypes;
pub mod validator;
pub use validator::OpExecutionPayloadValidator;
