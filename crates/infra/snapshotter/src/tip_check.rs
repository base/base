//! Pre-snapshot guard that aborts when the local EL is not at chain tip.

use std::time::Duration;

use alloy_provider::{Provider, ProviderBuilder};
use alloy_rpc_client::RpcClient;
use alloy_transport_http::Http;
use anyhow::{Context, Result, bail};
use tracing::info;
use url::Url;

/// Per-request timeout for the block-number RPC calls. Kept short so a wedged
/// or unreachable RPC fails the pre-check fast rather than hanging the cron job.
const RPC_TIMEOUT: Duration = Duration::from_secs(5);

/// Outcome of comparing the local EL head against the reference tip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TipStatus {
    /// Local head is within `tolerance` blocks of the reference (or ahead).
    AtTip {
        /// Signed delta `reference - local`. Positive means behind, negative ahead.
        delta_blocks: i64,
    },
    /// Local head is more than `tolerance` blocks behind the reference.
    Behind {
        /// Number of blocks the local head trails the reference by.
        behind_blocks: u64,
    },
}

/// Pre-snapshot tip check: verifies the local EL is caught up to the chain tip
/// before any destructive snapshot work begins.
#[derive(Debug)]
pub struct TipCheck;

impl TipCheck {
    /// Fetches the local EL head and the reference tip, then aborts with an
    /// error if the local node is more than `tolerance` blocks behind.
    ///
    /// Both heads are read via `eth_blockNumber`. The reference fetch is
    /// required (not best-effort): if the tip cannot be determined we cannot
    /// prove the node is at tip, so we refuse to snapshot.
    pub async fn ensure_at_tip(
        el_rpc: &Url,
        tip_reference_rpc: &Url,
        tolerance: u64,
    ) -> Result<()> {
        let (local_result, reference_result) = tokio::join!(
            Self::fetch_block_number(el_rpc),
            Self::fetch_block_number(tip_reference_rpc)
        );

        let local = local_result?;
        let reference = reference_result?;

        match Self::classify(local, reference, tolerance) {
            TipStatus::AtTip { delta_blocks } => {
                info!(
                    local,
                    reference, delta_blocks, tolerance, "EL is at tip; proceeding with snapshot"
                );
                Ok(())
            }
            TipStatus::Behind { behind_blocks } => {
                bail!(
                    "EL not at tip: local head #{local} is {behind_blocks} blocks behind \
                     reference tip #{reference} (tolerance {tolerance}). Refusing to snapshot."
                )
            }
        }
    }

    /// Pure delta classification, split out so the threshold logic is testable
    /// without any RPC. `delta = reference - local`; positive means the local
    /// node is behind. Saturating signed conversion keeps absurd RPC values
    /// from panicking; real chain heights are well under `i64::MAX`.
    fn classify(local: u64, reference: u64, tolerance: u64) -> TipStatus {
        let local_i = i64::try_from(local).unwrap_or(i64::MAX);
        let reference_i = i64::try_from(reference).unwrap_or(i64::MAX);
        let tolerance_i = i64::try_from(tolerance).unwrap_or(i64::MAX);
        let delta = reference_i.saturating_sub(local_i);

        if delta > tolerance_i {
            TipStatus::Behind { behind_blocks: delta.unsigned_abs() }
        } else {
            TipStatus::AtTip { delta_blocks: delta }
        }
    }

    /// Reads the latest block height from an EL RPC via `eth_blockNumber`.
    ///
    /// Uses a stock alloy provider with no network-specific block typing: the
    /// height is a plain scalar, so we avoid pulling in a chain-specific
    /// network type just to read a number. The HTTP client carries
    /// `RPC_TIMEOUT` so an unreachable endpoint fails fast.
    /// TODO: refactor this into a shared common crate
    async fn fetch_block_number(rpc: &Url) -> Result<u64> {
        let http_client = reqwest::Client::builder()
            .timeout(RPC_TIMEOUT)
            .build()
            .with_context(|| format!("building HTTP client for {rpc}"))?;
        let transport = Http::with_client(http_client, rpc.clone());
        let provider = ProviderBuilder::new()
            .disable_recommended_fillers()
            .connect_client(RpcClient::new(transport, false));
        provider
            .get_block_number()
            .await
            .with_context(|| format!("fetching eth_blockNumber from {rpc}"))
    }
}

#[cfg(test)]
mod tests {
    use super::{TipCheck, TipStatus};

    #[test]
    fn at_tip_when_local_equals_reference() {
        assert_eq!(
            TipCheck::classify(1_000, 1_000, 5),
            TipStatus::AtTip { delta_blocks: 0 },
            "equal heads must be treated as at tip"
        );
    }

    #[test]
    fn at_tip_when_behind_within_tolerance() {
        assert_eq!(
            TipCheck::classify(995, 1_000, 5),
            TipStatus::AtTip { delta_blocks: 5 },
            "exactly tolerance blocks behind must still be at tip"
        );
    }

    #[test]
    fn behind_when_exceeds_tolerance_by_one() {
        assert_eq!(
            TipCheck::classify(994, 1_000, 5),
            TipStatus::Behind { behind_blocks: 6 },
            "one block past tolerance must be classified behind"
        );
    }

    #[test]
    fn at_tip_when_local_ahead_of_reference() {
        assert_eq!(
            TipCheck::classify(1_010, 1_000, 5),
            TipStatus::AtTip { delta_blocks: -10 },
            "local ahead of reference (negative delta) must never be behind"
        );
    }

    #[test]
    fn zero_tolerance_requires_exact_tip() {
        assert_eq!(
            TipCheck::classify(999, 1_000, 0),
            TipStatus::Behind { behind_blocks: 1 },
            "with zero tolerance a single block behind must abort"
        );
        assert_eq!(
            TipCheck::classify(1_000, 1_000, 0),
            TipStatus::AtTip { delta_blocks: 0 },
            "with zero tolerance an exact match must pass"
        );
    }
}
