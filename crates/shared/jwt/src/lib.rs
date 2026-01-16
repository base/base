#![doc = include_str!("../README.md")]
#![doc(issue_tracker_base_url = "https://github.com/base/base/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod error;
pub use error::{JwtError, JwtValidationError};

mod secret;
pub use secret::{default_jwt_secret, read_jwt_secret, resolve_jwt_secret};

mod validator;
pub use alloy_rpc_types_engine::JwtSecret;
pub use validator::JwtValidator;

#[cfg(any(test, feature = "test-utils"))]
pub mod test_utils;
