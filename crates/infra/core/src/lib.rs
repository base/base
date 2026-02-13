#![doc = include_str!("../README.md")]
#![doc(issue_tracker_base_url = "https://github.com/base/tips/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
use alloy_rpc_types as _;

mod kafka;
pub use kafka::load_kafka_config_from_file;

mod logger;
pub use logger::{LogFormat, init_logger, init_logger_with_format};

mod metrics;
pub use metrics::init_prometheus_exporter;

/// Test utilities and fixtures.
#[cfg(any(test, feature = "test-utils"))]
pub mod test_utils;

mod types;
pub use types::{
    AcceptedBundle, Bundle, BundleExtensions, BundleHash, BundleTxs, CancelBundle,
    MeterBundleResponse, ParsedBundle, TransactionResult,
};
