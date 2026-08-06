//! Verifies the execution layer is caught up to chain tip before pausing.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use alloy_eips::BlockNumberOrTag;
use alloy_provider::{Provider, ProviderBuilder};
use anyhow::{Context, Result};
use async_trait::async_trait;
use tracing::info;
use url::Url;

/// Result of checking an execution layer node's latest block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TipStatus {
    /// Number of the latest block returned by the EL.
    pub block_number: u64,
    /// Whether the latest block is within the configured freshness threshold.
    pub at_tip: bool,
}

/// Checks whether an execution layer node is at chain tip and reports its block height.
///
/// Abstracted behind a trait (like [`crate::ContainerManager`]) so the
/// orchestrator can be exercised in tests without a live RPC endpoint.
#[async_trait]
pub trait TipChecker: Send + Sync {
    /// Returns the EL's latest block number and whether it is within `threshold`
    /// of the current wall-clock time.
    async fn check_tip(&self, threshold: Duration) -> Result<TipStatus>;
}

/// [`TipChecker`] backed by an execution layer JSON-RPC endpoint.
///
/// Determines tip status by fetching the `latest` block via
/// `eth_getBlockByNumber` and comparing its timestamp against the current
/// wall-clock time.
#[derive(Debug, Clone)]
pub struct RpcTipChecker {
    rpc_url: Url,
}

impl RpcTipChecker {
    /// Creates a new tip checker targeting the given EL RPC URL.
    pub const fn new(rpc_url: Url) -> Self {
        Self { rpc_url }
    }
}

#[async_trait]
impl TipChecker for RpcTipChecker {
    async fn check_tip(&self, threshold: Duration) -> Result<TipStatus> {
        let provider = ProviderBuilder::new()
            .disable_recommended_fillers()
            .connect(self.rpc_url.as_str())
            .await
            .with_context(|| format!("connecting to EL RPC at {}", self.rpc_url))?;

        let block = provider
            .get_block_by_number(BlockNumberOrTag::Latest)
            .await
            .context("fetching latest block")?
            .context("latest block not found")?;

        let block_timestamp = block.header.timestamp;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("system clock is before UNIX epoch")?
            .as_secs();

        // Saturating: a block timestamp in the future (clock skew) yields an age
        // of 0, which is always within threshold.
        let age = now.saturating_sub(block_timestamp);
        let at_tip = age <= threshold.as_secs();

        info!(
            block = block.header.number,
            block_timestamp,
            now,
            age_secs = age,
            threshold_secs = threshold.as_secs(),
            at_tip,
            "checked EL tip status"
        );

        Ok(TipStatus { block_number: block.header.number, at_tip })
    }
}
