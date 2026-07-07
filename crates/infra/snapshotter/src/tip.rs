//! Verifies the execution layer is caught up to chain tip before pausing.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use alloy_eips::BlockNumberOrTag;
use alloy_provider::{Provider, ProviderBuilder};
use anyhow::{Context, Result};
use tracing::info;
use url::Url;

/// Checks whether an execution layer node is at chain tip by inspecting the
/// timestamp of its latest block.
#[derive(Debug)]
pub struct TipChecker;

impl TipChecker {
    /// Returns `true` if the EL's latest block is within `threshold` of the
    /// current wall-clock time.
    ///
    /// Fetches the `latest` block via `eth_getBlockByNumber` and compares its
    /// timestamp against `SystemTime::now()`. A block in the future (clock skew)
    /// is always considered at tip.
    pub async fn is_at_tip(rpc_url: &Url, threshold: Duration) -> Result<bool> {
        let provider = ProviderBuilder::new()
            .disable_recommended_fillers()
            .connect(rpc_url.as_str())
            .await
            .with_context(|| format!("connecting to EL RPC at {rpc_url}"))?;

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

        Ok(at_tip)
    }
}
