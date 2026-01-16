#![doc = include_str!("../README.md")]
#![doc(issue_tracker_base_url = "https://github.com/base/base/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

#[cfg(any(test, feature = "test-utils"))]
pub mod test_utils;

// These dependencies are only used in test-utils feature
#[cfg(any(test, feature = "test-utils"))]
pub use test_utils::*;
