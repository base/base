//! RPC client for querying devnet nodes.

use alloy_primitives::{Address, U256};
use alloy_provider::{Provider, RootProvider};
use base_consensus_rpc::SyncStatusApiClient;
use base_protocol::SyncStatus;
use eyre::{Result, WrapErr};
use jsonrpsee::http_client::{HttpClient, HttpClientBuilder};

/// RPC client for querying devnet L1 and L2 nodes.
#[derive(Debug)]
pub struct DevnetRpcClient {
    l1_provider: RootProvider,
    l2_builder_provider: RootProvider,
    l2_client_provider: RootProvider,
    l2_builder_op_client: HttpClient,
    l2_client_op_client: HttpClient,
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
        let l2_builder_op_client = Self::create_http_client(l2_builder_op_rpc_url)?;
        let l2_client_op_client = Self::create_http_client(l2_client_op_rpc_url)?;

        Ok(Self {
            l1_provider,
            l2_builder_provider,
            l2_client_provider,
            l2_builder_op_client,
            l2_client_op_client,
        })
    }

    /// Create a provider from an HTTP URL.
    fn create_provider(url: &str) -> Result<RootProvider> {
        let url: url::Url = url.parse().wrap_err("Invalid URL")?;
        Ok(RootProvider::new_http(url))
    }

    /// Create a jsonrpsee HTTP client.
    fn create_http_client(url: &str) -> Result<HttpClient> {
        HttpClientBuilder::default().build(url).wrap_err("Failed to create HTTP client")
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
        self.l2_builder_op_client
            .op_sync_status()
            .await
            .wrap_err("Failed to get L2 builder sync status")
    }

    /// Get sync status from L2 client op-node.
    pub async fn l2_client_sync_status(&self) -> Result<SyncStatus> {
        self.l2_client_op_client
            .op_sync_status()
            .await
            .wrap_err("Failed to get L2 client sync status")
    }
}
