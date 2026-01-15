#![doc = include_str!("../README.md")]
#![doc(issue_tracker_base_url = "https://github.com/base/node-reth/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

#[macro_use]
extern crate tracing;

mod events;
pub use events::{EventLog, Pool, TxEvent};

mod subscription;
pub use subscription::tracex_subscription;

mod rpc;
pub use rpc::{
    Status, TransactionStatusApiImpl, TransactionStatusApiServer, TransactionStatusResponse,
};

mod tracker;
pub use tracker::Tracker;

mod extension;
pub use extension::{TxPoolExtension, TxpoolConfig};

mod metrics;
pub use metrics::Metrics;
