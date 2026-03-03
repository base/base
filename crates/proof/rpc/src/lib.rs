#![doc = include_str!("../README.md")]
#![doc(issue_tracker_base_url = "https://github.com/base/base/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]

use alloy_network::Ethereum;
use alloy_provider::RootProvider;
use base_alloy_network::Base;

/// Shared type alias for the L1 HTTP provider.
/// Uses `RootProvider` directly since these clients only perform read operations.
pub type HttpProvider = RootProvider<Ethereum>;

/// L2-specific provider type using the Base network.
/// Required for deserializing OP Stack deposit transactions (type 0x7E).
pub type L2HttpProvider = RootProvider<Base>;

mod config;
pub use config::{DEFAULT_CACHE_SIZE, RetryConfig};

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
pub use types::{GenesisL2BlockRef, L1BlockId, L1BlockRef, L2BlockRef, OpBlock, SyncStatus};
