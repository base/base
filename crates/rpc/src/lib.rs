#![doc = include_str!("../README.md")]
#![doc(issue_tracker_base_url = "https://github.com/base/node-reth/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod traits;
pub use traits::{MeteringApiServer, TransactionStatusApiServer};

mod transaction_rpc;
pub use transaction_rpc::TransactionStatusApiImpl;

mod types;
// Re-export tips core types
pub use tips_core::types::{Bundle, MeterBundleResponse, TransactionResult};
pub use types::{Status, TransactionStatusResponse};

mod meter;
pub use meter::meter_bundle;

mod meter_rpc;
pub use meter_rpc::MeteringApiImpl;

#[cfg(test)]
mod tests;
