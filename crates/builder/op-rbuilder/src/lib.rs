#![allow(missing_docs)]
#![doc = include_str!("../README.md")]
#![doc(issue_tracker_base_url = "https://github.com/base/base/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), allow(unused_crate_dependencies))]

pub mod flashblocks;
pub mod launcher;
pub mod metrics;
pub mod primitives;
pub mod traits;
pub mod tx_data_store;

#[cfg(any(test, feature = "test-utils"))]
pub mod test_utils;
