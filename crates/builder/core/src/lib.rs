#![allow(missing_docs)]
#![doc = include_str!("../README.md")]
#![doc(issue_tracker_base_url = "https://github.com/base/base/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), allow(unused_crate_dependencies))]

mod execution;
pub use execution::{ExecutionInfo, TxnExecutionError, TxnOutcome};

mod engine;
pub use engine::OpEngineApiBuilder;

mod metrics;
pub use metrics::OpRBuilderMetrics;

mod traits;
pub use traits::{ClientBounds, NodeBounds, NodeComponents, PayloadTxsBounds, PoolBounds};

mod launcher;
pub use launcher::{BuilderLauncher, launch};

mod storage;
pub use storage::{
    BaseApiExtServer, StoreData, StoredBackrunBundle, TxData, TxDataStore, TxDataStoreExt,
};

pub mod flashblocks;

#[cfg(any(test, feature = "test-utils"))]
pub mod test_utils;
