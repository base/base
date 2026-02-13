#![doc = include_str!("../README.md")]
#![doc(issue_tracker_base_url = "https://github.com/base/tips/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
use alloy_rpc_types as _;

/// Kafka configuration utilities.
pub mod kafka;
/// Logging initialization and format configuration.
pub mod logger;
/// Prometheus metrics exporter.
pub mod metrics;
/// Test utilities and fixtures.
#[cfg(any(test, feature = "test-utils"))]
pub mod test_utils;
/// Core bundle and transaction types.
pub mod types;

pub use types::{
    AcceptedBundle, Bundle, BundleExtensions, BundleHash, BundleTxs, CancelBundle,
    MeterBundleResponse,
};
