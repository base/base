#![doc = include_str!("../README.md")]
#![doc(issue_tracker_base_url = "https://github.com/base/node-reth/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

#[cfg(any(test, feature = "test-utils"))]
pub mod test_utils;

#[cfg(feature = "op-rbuilder")]
pub mod op_rbuilder;

#[cfg(any(test, feature = "test-utils"))]
pub use test_utils::*;
