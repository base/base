#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    issue_tracker_base_url = "https://github.com/base/base/issues/"
)]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod error;
pub use error::{RpcErrorClassifier, TxManagerError, TxManagerResult};

mod candidate;
pub use candidate::TxCandidate;

mod fees;
pub use fees::{BumpedFees, FeeCalculator, FeeOverride, GasPriceCaps};

mod send_state;
pub use send_state::SendState;

mod macros;

mod config;
pub use config::{ConfigError, GweiParser, TxManagerConfig};

mod signer_config;
pub use signer_config::SignerConfig;

mod traits;
pub use traits::{SendHandle, SendResponse, TxManager};

mod nonce;
pub use nonce::{NonceGuard, NonceManager, NonceState};

mod manager;
pub use manager::{PreparedTx, SimpleTxManager};

mod queue;
pub use queue::{SendResult, TxQueue};

mod metrics;
pub use metrics::{BaseTxMetrics, NoopTxMetrics, TxMetrics};

mod blob;
pub use blob::{BlobTxBuilder, MAX_BLOBS_PER_TX};

#[cfg(test)]
pub mod test_utils;
