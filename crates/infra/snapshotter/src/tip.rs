//! Verifies the execution layer is caught up to chain tip before pausing.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use alloy_eips::BlockNumberOrTag;
use alloy_provider::{Provider, ProviderBuilder};
use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;
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
#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub trait TipChecker: Send + Sync {
    /// Returns the EL's latest block number and whether it is within `threshold`
    /// of the current wall-clock time.
    async fn check_tip(&self, threshold: Duration) -> Result<TipStatus>;

    /// Returns the latest block with proofs, or `None` if the proofs database is empty.
    async fn proofs_latest(&self) -> Result<Option<u64>>;
}

/// Latest proofs block returned by `debug_proofsSyncStatus`.
///
/// Uses a lightweight response instead of depending on the execution node's RPC implementation.
#[derive(Debug, Deserialize)]
pub struct ProofsSyncStatus {
    /// Latest block with proofs; `None` when the proofs database is empty.
    pub latest: Option<u64>,
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

    async fn proofs_latest(&self) -> Result<Option<u64>> {
        let provider = ProviderBuilder::new()
            .disable_recommended_fillers()
            .connect(self.rpc_url.as_str())
            .await
            .with_context(|| format!("connecting to EL RPC at {}", self.rpc_url))?;
        let status: ProofsSyncStatus = provider
            .raw_request("debug_proofsSyncStatus".into(), ())
            .await
            .context("fetching proofs sync status; ensure the proofs ExEx and debug RPC are enabled")?;
        Ok(status.latest)
    }
}

#[cfg(test)]
mod tests {
    use jsonrpsee::{RpcModule, server::ServerBuilder, types::ErrorObjectOwned};
    use serde_json::json;

    use super::*;

    /// Verifies the RPC method name, empty params, and numeric/null response decoding.
    #[tokio::test]
    async fn proofs_latest_reads_rpc_status() {
        for latest in [Some(0u64), Some(42), None] {
            let server = ServerBuilder::default()
                .build("127.0.0.1:0")
                .await
                .expect("RPC test server should bind");
            let addr = server.local_addr().expect("RPC server should have an address");
            let mut module = RpcModule::new(());
            module
                .register_method("debug_proofsSyncStatus", move |params, _, _| {
                    assert!(
                        matches!(params.as_str(), None | Some("[]" | "null")),
                        "proofs status RPC must have no parameters"
                    );
                    json!({"earliest": 0, "latest": latest})
                })
                .expect("proofs status method should register");
            let handle = server.start(module);
            let checker = RpcTipChecker::new(format!("http://{addr}").parse().expect("valid RPC URL"));
            assert_eq!(
                checker.proofs_latest().await.expect("proofs status should decode"),
                latest,
                "checker must return the exact numeric or empty proofs head"
            );
            handle.stop().expect("test server should stop");
            handle.stopped().await;
        }
    }

    /// Verifies unsupported RPC methods and malformed responses fail closed.
    #[tokio::test]
    async fn proofs_latest_rejects_rpc_errors_and_malformed_status() {
        for response in [
            Err(ErrorObjectOwned::owned(-32601, "Method not found", None::<()>)),
            Ok(json!({"latest": "invalid"})),
        ] {
            let server = ServerBuilder::default()
                .build("127.0.0.1:0")
                .await
                .expect("RPC test server should bind");
            let addr = server.local_addr().expect("RPC server should have an address");
            let mut module = RpcModule::new(());
            module
                .register_method("debug_proofsSyncStatus", move |_, _, _| response.clone())
                .expect("proofs status method should register");
            let handle = server.start(module);
            let checker = RpcTipChecker::new(format!("http://{addr}").parse().expect("valid RPC URL"));
            let err = checker.proofs_latest().await.expect_err("unknown proofs status must be rejected");
            assert_eq!(
                err.to_string(),
                "fetching proofs sync status; ensure the proofs ExEx and debug RPC are enabled",
                "RPC and decoding errors must identify the proofs status request"
            );
            handle.stop().expect("test server should stop");
            handle.stopped().await;
        }
    }
}
