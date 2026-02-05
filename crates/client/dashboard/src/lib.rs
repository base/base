//! Monitoring dashboard for Base node.
//!
//! This crate provides a real-time web dashboard for visualizing execution layer metrics:
//! - Gas prices and transaction pool statistics
//! - P2P peer connections
//! - Block transactions and timing
//! - System resource usage (CPU, memory)
//!
//! The dashboard uses Server-Sent Events (SSE) for real-time data streaming to a D3.js-based
//! frontend.

#![doc(issue_tracker_base_url = "https://github.com/base/base/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod collectors;
mod data_feed;
mod extension;
mod server;
mod types;

pub use extension::{DashboardConfig, DashboardExtension};
pub use types::{
    BlockForWeb, DashboardEvent, ForkChoiceData, LogForWeb, NodeData, PeerData, ProcessedData,
    ReceiptForWeb, SystemData, TransactionForWeb, TxLink, TxNode, TxPoolData,
};
