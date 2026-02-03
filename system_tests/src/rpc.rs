//! RPC client for querying devnet nodes.

use alloy_network::Ethereum;
use alloy_primitives::{Address, U256};
use alloy_provider::{Provider, RootProvider};
use alloy_rpc_client::RpcClient;
use eyre::{Result, WrapErr};
use serde::Deserialize;

/// Creates an HTTP provider for the given RPC URL.
pub fn http_provider(url: &str) -> Result<RootProvider<Ethereum>> {
    let client = RpcClient::builder().http(url.parse()?);
    Ok(RootProvider::<Ethereum>::new(client))
}

/// Sync status from op-node's `optimism_syncStatus` RPC.
#[derive(Debug, Clone, Deserialize)]
pub struct SyncStatus {
    /// Unsafe L2 block info.
    pub unsafe_l2: Option<L2BlockRef>,
    /// Safe L2 block info.
    pub safe_l2: Option<L2BlockRef>,
}

/// L2 block reference from sync status.
#[derive(Debug, Clone, Deserialize)]
pub struct L2BlockRef {
    /// Block number.
    pub number: u64,
}

/// RPC client for querying devnet L1 and L2 nodes.
#[derive(Debug)]
pub struct DevnetRpcClient {
    l1_provider: RootProvider,
    l2_builder_provider: RootProvider,
    l2_client_provider: RootProvider,
    l2_builder_op_rpc_url: String,
    l2_client_op_rpc_url: String,
}

impl DevnetRpcClient {
    /// Create a new `DevnetRpcClient` with L1, L2 builder, and L2 client endpoints.
    pub fn new(
        l1_url: &str,
        l2_builder_url: &str,
        l2_client_url: &str,
        l2_builder_op_rpc_url: &str,
        l2_client_op_rpc_url: &str,
    ) -> Result<Self> {
        let l1_provider = Self::create_provider(l1_url)?;
        let l2_builder_provider = Self::create_provider(l2_builder_url)?;
        let l2_client_provider = Self::create_provider(l2_client_url)?;

        Ok(Self {
            l1_provider,
            l2_builder_provider,
            l2_client_provider,
            l2_builder_op_rpc_url: l2_builder_op_rpc_url.to_string(),
            l2_client_op_rpc_url: l2_client_op_rpc_url.to_string(),
        })
    }

    /// Create a provider from an HTTP URL.
    fn create_provider(url: &str) -> Result<RootProvider> {
        let url: url::Url = url.parse().wrap_err("Invalid URL")?;
        Ok(RootProvider::new_http(url))
    }

    /// Get the current block number on L1.
    pub async fn l1_block_number(&self) -> Result<u64> {
        self.l1_provider.get_block_number().await.wrap_err("Failed to get L1 block number")
    }

    /// Get the current block number on L2 builder.
    pub async fn l2_builder_block_number(&self) -> Result<u64> {
        self.l2_builder_provider
            .get_block_number()
            .await
            .wrap_err("Failed to get L2 builder block number")
    }

    /// Get the current block number on L2 client.
    pub async fn l2_client_block_number(&self) -> Result<u64> {
        self.l2_client_provider
            .get_block_number()
            .await
            .wrap_err("Failed to get L2 client block number")
    }

    /// Get the balance of an address across all three nodes.
    pub async fn get_balance(&self, address: Address) -> Result<(U256, U256, U256)> {
        let l1 = self.l1_provider.get_balance(address).await.wrap_err("L1 balance")?;
        let l2_builder =
            self.l2_builder_provider.get_balance(address).await.wrap_err("L2 builder balance")?;
        let l2_client =
            self.l2_client_provider.get_balance(address).await.wrap_err("L2 client balance")?;
        Ok((l1, l2_builder, l2_client))
    }

    /// Get sync status from L2 builder op-node.
    pub async fn l2_builder_sync_status(&self) -> Result<SyncStatus> {
        self.fetch_sync_status(&self.l2_builder_op_rpc_url).await
    }

    /// Get sync status from L2 client op-node.
    pub async fn l2_client_sync_status(&self) -> Result<SyncStatus> {
        self.fetch_sync_status(&self.l2_client_op_rpc_url).await
    }

    async fn fetch_sync_status(&self, url: &str) -> Result<SyncStatus> {
        let client = reqwest::Client::new();
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "optimism_syncStatus",
            "params": [],
            "id": 1
        });

        let resp = client
            .post(url)
            .json(&body)
            .send()
            .await
            .wrap_err("Failed to send syncStatus request")?;

        #[derive(Deserialize)]
        struct RpcResponse {
            result: SyncStatus,
        }

        let rpc_resp: RpcResponse =
            resp.json().await.wrap_err("Failed to parse syncStatus response")?;
        Ok(rpc_resp.result)
    }
}
