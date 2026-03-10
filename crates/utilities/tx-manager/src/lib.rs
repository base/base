#![doc = include_str!("../README.md")]
#![doc(issue_tracker_base_url = "https://github.com/base/base/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod error;
pub use error::{RpcErrorClassifier, TxManagerError, TxManagerResult};

mod candidate;
pub use candidate::TxCandidate;

mod fees;
pub use fees::FeeCalculator;

mod send_state;
pub use send_state::SendState;

mod config;
pub use config::TxManagerConfig;

mod traits;
pub use traits::TxManager;

mod nonce;
pub use nonce::NonceManager;

mod manager;
pub use manager::SimpleTxManager;

mod queue;
pub use queue::TxQueue;

mod metrics;
pub use metrics::TxMetrics;

mod blob;
pub use blob::BlobTxBuilder;
