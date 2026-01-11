#![doc = include_str!("../README.md")]
#![doc(issue_tracker_base_url = "https://github.com/base/node-reth/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod block;
pub use block::meter_block;

mod extension;
pub use extension::{MeteringRpcConfig, MeteringRpcExtension};

mod meter;
pub use meter::{FlashblocksState, MeterBundleOutput, meter_bundle};

mod rpc;
pub use rpc::MeteringApiImpl;

mod traits;
pub use traits::MeteringApiServer;

mod types;
pub use types::{MeterBlockResponse, MeterBlockTransactions};

#[cfg(any(test, feature = "test-utils"))]
pub mod test_utils;
