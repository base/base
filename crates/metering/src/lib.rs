#![doc = include_str!("../README.md")]
#![doc(issue_tracker_base_url = "https://github.com/base/node-reth/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

// Re-export tips core types
pub use tips_core::types::{Bundle, MeterBundleResponse, TransactionResult};

mod meter;
pub use meter::meter_bundle;

mod rpc;
pub use rpc::{MeteringApiImpl, MeteringApiServer};

#[cfg(test)]
mod tests;
