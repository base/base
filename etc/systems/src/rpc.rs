//! RPC clients and provider helpers for querying system test nodes.

use std::time::{Duration, Instant};

use alloy_eips::BlockNumberOrTag;
use alloy_network::Network;
use alloy_primitives::{Address, B256, U256};
use alloy_provider::{Provider, RootProvider};
use async_trait::async_trait;
use base_common_network::Base;
use base_common_rpc_types::BaseTransactionReceipt;
use base_consensus_rpc::SyncStatusApiClient;
use base_protocol::SyncStatus;
use eyre::{Result, WrapErr};
use jsonrpsee::http_client::{HttpClient, HttpClientBuilder};
use tokio::time::{sleep, timeout};

const BLOCK_POLL_INTERVAL: Duration = Duration::from_millis(500);
const RECEIPT_POLL_INTERVAL: Duration = Duration::from_millis(100);
const CONVERGENCE_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Polling helpers for Base providers used by system tests.
#[async_trait]
pub trait SystemTestProviderExt {
    /// Waits until the provider reaches at least `min_block`.
    async fn wait_for_block(&self, min_block: u64, within: Duration) -> Result<u64>;

    /// Waits until `address` has a non-zero balance.
    async fn wait_for_balance(&self, address: Address, within: Duration) -> Result<()>;

    /// Waits for a transaction receipt.
    async fn wait_for_receipt(
        &self,
        tx_hash: B256,
        within: Duration,
    ) -> Result<BaseTransactionReceipt>;

    /// Waits for a transaction to be included above `previous_height`.
    async fn wait_for_receipt_after(
        &self,
        tx_hash: B256,
        previous_height: u64,
        within: Duration,
    ) -> Result<BaseTransactionReceipt>;

    /// Verifies that no transaction receipt appears during `window`.
    async fn assert_receipt_absent(&self, tx_hash: B256, window: Duration) -> Result<()>;

    /// Returns the block hash at `height`, if the block exists.
    async fn block_hash_at(&self, height: u64) -> Result<Option<B256>>;

    /// Waits for the block hash at `height` to become available.
    async fn wait_for_block_hash_at(&self, height: u64, within: Duration) -> Result<B256>;

    /// Waits until this provider and `canonical` agree on the block at `height`.
    async fn wait_for_convergence(
        &self,
        canonical: &RootProvider<Base>,
        height: u64,
        within: Duration,
    ) -> Result<()>;
}

#[async_trait]
impl SystemTestProviderExt for RootProvider<Base> {
    async fn wait_for_block(&self, min_block: u64, within: Duration) -> Result<u64> {
        timeout(within, async {
            loop {
                let block = self.get_block_number().await?;
                if block >= min_block {
                    return Ok::<_, eyre::Error>(block);
                }
                sleep(BLOCK_POLL_INTERVAL).await;
            }
        })
        .await
        .wrap_err("block production timed out")?
    }

    async fn wait_for_balance(&self, address: Address, within: Duration) -> Result<()> {
        timeout(within, async {
            loop {
                if self.get_balance(address).await? > U256::ZERO {
                    return Ok::<_, eyre::Error>(());
                }
                sleep(BLOCK_POLL_INTERVAL).await;
            }
        })
        .await
        .wrap_err("timed out waiting for a funded account")?
    }

    async fn wait_for_receipt(
        &self,
        tx_hash: B256,
        within: Duration,
    ) -> Result<BaseTransactionReceipt> {
        timeout(within, async {
            loop {
                if let Some(receipt) = self.get_transaction_receipt(tx_hash).await? {
                    return Ok::<_, eyre::Error>(receipt);
                }
                sleep(RECEIPT_POLL_INTERVAL).await;
            }
        })
        .await
        .wrap_err("transaction receipt timed out")?
    }

    async fn wait_for_receipt_after(
        &self,
        tx_hash: B256,
        previous_height: u64,
        within: Duration,
    ) -> Result<BaseTransactionReceipt> {
        timeout(within, async {
            loop {
                if let Some(receipt) = self.get_transaction_receipt(tx_hash).await?
                    && receipt.inner.block_number.is_some_and(|height| height > previous_height)
                {
                    return Ok::<_, eyre::Error>(receipt);
                }
                sleep(RECEIPT_POLL_INTERVAL).await;
            }
        })
        .await
        .wrap_err("transaction was not re-included after reconciliation")?
    }

    async fn assert_receipt_absent(&self, tx_hash: B256, window: Duration) -> Result<()> {
        let deadline = Instant::now() + window;
        while Instant::now() < deadline {
            if self.get_transaction_receipt(tx_hash).await?.is_some() {
                eyre::bail!("transaction was included when it should not have been");
            }
            sleep(BLOCK_POLL_INTERVAL).await;
        }
        Ok(())
    }

    async fn block_hash_at(&self, height: u64) -> Result<Option<B256>> {
        Ok(self
            .get_block_by_number(BlockNumberOrTag::Number(height))
            .await?
            .map(|block| block.header.hash))
    }

    async fn wait_for_block_hash_at(&self, height: u64, within: Duration) -> Result<B256> {
        timeout(within, async {
            loop {
                if let Some(hash) = self.block_hash_at(height).await? {
                    return Ok::<_, eyre::Error>(hash);
                }
                sleep(BLOCK_POLL_INTERVAL).await;
            }
        })
        .await
        .wrap_err("block at target height not available in time")?
    }

    async fn wait_for_convergence(
        &self,
        canonical: &RootProvider<Base>,
        height: u64,
        within: Duration,
    ) -> Result<()> {
        timeout(within, async {
            loop {
                let canonical_hash = canonical.block_hash_at(height).await?;
                let local_hash = self.block_hash_at(height).await?;
                if let (Some(canonical_hash), Some(local_hash)) = (canonical_hash, local_hash)
                    && canonical_hash == local_hash
                {
                    return Ok::<_, eyre::Error>(());
                }
                sleep(CONVERGENCE_POLL_INTERVAL).await;
            }
        })
        .await
        .wrap_err("chain did not converge to canonical at the target height")?
    }
}

/// RPC client for querying system test L1 and L2 nodes.
#[derive(Debug)]
pub struct SystemTestRpcClient {
    l1_provider: RootProvider,
    l2_builder_provider: RootProvider<Base>,
    l2_client_provider: RootProvider<Base>,
    l2_builder_consensus_client: HttpClient,
    l2_client_consensus_client: HttpClient,
}

impl SystemTestRpcClient {
    /// Create a new `SystemTestRpcClient` with L1, L2 builder, and L2 client endpoints.
    pub fn new(
        l1_url: &str,
        l2_builder_url: &str,
        l2_client_url: &str,
        l2_builder_consensus_rpc_url: &str,
        l2_client_consensus_rpc_url: &str,
    ) -> Result<Self> {
        let l1_provider = Self::create_provider(l1_url)?;
        let l2_builder_provider = Self::create_provider(l2_builder_url)?;
        let l2_client_provider = Self::create_provider(l2_client_url)?;
        let l2_builder_consensus_client = Self::create_http_client(l2_builder_consensus_rpc_url)?;
        let l2_client_consensus_client = Self::create_http_client(l2_client_consensus_rpc_url)?;

        Ok(Self {
            l1_provider,
            l2_builder_provider,
            l2_client_provider,
            l2_builder_consensus_client,
            l2_client_consensus_client,
        })
    }

    /// Create a provider from an HTTP URL.
    fn create_provider<N: Network>(url: &str) -> Result<RootProvider<N>> {
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

    /// Get sync status from the L2 builder consensus node.
    pub async fn l2_builder_sync_status(&self) -> Result<SyncStatus> {
        self.l2_builder_consensus_client
            .sync_status()
            .await
            .wrap_err("Failed to get L2 builder sync status")
    }

    /// Get sync status from the L2 client consensus node.
    pub async fn l2_client_sync_status(&self) -> Result<SyncStatus> {
        self.l2_client_consensus_client
            .sync_status()
            .await
            .wrap_err("Failed to get L2 client sync status")
    }
}
