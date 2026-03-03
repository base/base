#![doc = include_str!("../README.md")]
#![doc(issue_tracker_base_url = "https://github.com/base/base/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]

mod config;
pub use config::RetryConfig;

mod cache;
pub use cache::{CacheMetrics, MeteredCache};

mod error;
pub use error::{RpcError, RpcResult};

mod l1_client;
pub use l1_client::{L1ClientConfig, L1ClientImpl};

mod l2_client;
pub use l2_client::{L2ClientConfig, L2ClientImpl, ProofCacheKey};

mod rollup_client;
pub use rollup_client::{RollupClientConfig, RollupClientImpl};

mod traits;
pub use traits::{L1Client, L2Client, RollupClient};

mod types;
pub use types::{
    GenesisL2BlockRef, HttpProvider, L1BlockId, L1BlockRef, L2BlockRef, L2HttpProvider, OpBlock,
    SyncStatus,
};
