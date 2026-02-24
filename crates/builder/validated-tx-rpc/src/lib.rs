#![doc = include_str!("../README.md")]
#![doc(issue_tracker_base_url = "https://github.com/base/base/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

// Re-export types from shared primitives
pub use base_primitives::{
    ConversionError, InsertResult, TransactionSignature, ValidatedTransaction, base64_bytes,
};

mod rpc;
pub use rpc::{ValidatedTxApiImpl, ValidatedTxApiServer};

mod extension;
pub use extension::{ValidatedTxRpcConfig, ValidatedTxRpcExtension};
