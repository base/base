//! Transaction pool RPC APIs for Base.
//!
//! Provides RPC endpoints for querying transaction status and managing the transaction pool.

#![doc(issue_tracker_base_url = "https://github.com/base/base/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod rpc;
pub use rpc::{
    Status, TransactionStatusApiImpl, TransactionStatusApiServer, TransactionStatusResponse,
    TxPoolManagementApiImpl, TxPoolManagementApiServer,
};

mod extension;
pub use extension::{TxPoolRpcConfig, TxPoolRpcExtension};
