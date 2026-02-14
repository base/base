use alloy_network::Network;
use alloy_primitives::{Bytes, TxHash};
use alloy_provider::{Provider, RootProvider};
use anyhow::Result;
use base_primitives::{Bundle, BundleHash, CancelBundle};

/// Client for TIPS-specific RPC methods (`eth_sendBundle`, `eth_cancelBundle`)
///
/// Wraps a `RootProvider` to add TIPS functionality while preserving access
/// to standard Ethereum JSON-RPC methods via `provider()`.
#[derive(Debug, Clone)]
pub struct TipsRpcClient<N: Network = alloy_network::Ethereum> {
    provider: RootProvider<N>,
}

impl<N: Network> TipsRpcClient<N> {
    /// Creates a new client wrapping the given provider.
    pub const fn new(provider: RootProvider<N>) -> Self {
        Self { provider }
    }

    /// Sends a signed raw transaction via `eth_sendRawTransaction`.
    pub async fn send_raw_transaction(&self, signed_tx: Bytes) -> Result<TxHash> {
        let tx_hex = format!("0x{}", hex::encode(&signed_tx));
        self.provider
            .raw_request("eth_sendRawTransaction".into(), [tx_hex])
            .await
            .map_err(Into::into)
    }

    /// Sends a bundle via `eth_sendBundle`.
    pub async fn send_bundle(&self, bundle: Bundle) -> Result<BundleHash> {
        self.provider.raw_request("eth_sendBundle".into(), [bundle]).await.map_err(Into::into)
    }

    /// Sends a backrun bundle via `eth_sendBackrunBundle`.
    pub async fn send_backrun_bundle(&self, bundle: Bundle) -> Result<BundleHash> {
        self.provider
            .raw_request("eth_sendBackrunBundle".into(), [bundle])
            .await
            .map_err(Into::into)
    }

    /// Cancels a bundle via `eth_cancelBundle`.
    pub async fn cancel_bundle(&self, request: CancelBundle) -> Result<bool> {
        self.provider.raw_request("eth_cancelBundle".into(), [request]).await.map_err(Into::into)
    }

    /// Returns a reference to the underlying provider.
    pub const fn provider(&self) -> &RootProvider<N> {
        &self.provider
    }
}
