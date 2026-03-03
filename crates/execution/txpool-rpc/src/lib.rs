#![doc = include_str!("../README.md")]
#![doc(issue_tracker_base_url = "https://github.com/base/base/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod rpc;
pub use rpc::{
    AdminTxPoolApiImpl, AdminTxPoolApiServer, Status, TransactionStatusApiImpl,
    TransactionStatusApiServer, TransactionStatusResponse,
};

mod extension;
pub use extension::{TxPoolRpcConfig, TxPoolRpcExtension};
