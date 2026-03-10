#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    issue_tracker_base_url = "https://github.com/base/base/issues/"
)]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]

mod config;
pub use config::{
    DEFAULT_CACHE_SIZE, DEFAULT_RETRY_INITIAL_DELAY, DEFAULT_RETRY_MAX_DELAY,
    DEFAULT_RPC_MAX_RETRIES, RetryConfig,
};

mod cache;
pub use cache::{CacheMetrics, MeteredCache};

mod error;
pub use error::{RpcError, RpcResult};

mod traits;
pub use traits::{L1Provider, L2Provider, RollupProvider};

mod l1_client;
pub use l1_client::{L1Client, L1ClientConfig};

mod l2_client;
pub use l2_client::{L2Client, L2ClientConfig, ProofCacheKey};

mod rollup_client;
pub use rollup_client::{RollupClient, RollupClientConfig};

mod types;
pub use types::{
    GenesisL2BlockRef, HttpProvider, L1BlockId, L1BlockRef, L2BlockRef, L2HttpProvider, OpBlock,
    SyncStatus,
};
