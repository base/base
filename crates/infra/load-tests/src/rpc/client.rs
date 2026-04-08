use std::future::Future;

use alloy_network::{Ethereum, EthereumWallet};
use alloy_primitives::{Address, TxHash, U256};
use alloy_provider::{
    Identity, Provider, ProviderBuilder, RootProvider,
    fillers::{ChainIdFiller, FillProvider, JoinFill, WalletFiller},
};
use alloy_rpc_types::BlockNumberOrTag;
use base_alloy_network::Base;
use base_common_rpc_types::OpTransactionReceipt;
use tracing::instrument;
use url::Url;

use crate::utils::{BaselineError, Result};

/// Provider trait for fetching transaction receipts and block data.
///
/// This trait abstracts the RPC calls needed by the confirmer, enabling
/// mock implementations for testing.
pub trait ReceiptProvider: Send + Sync {
    /// Fetches transaction hashes from the pending block.
    fn get_pending_block_tx_hashes(&self) -> impl Future<Output = Result<Vec<TxHash>>> + Send;

    /// Fetches the transaction receipt for a given hash.
    fn get_transaction_receipt(
        &self,
        tx_hash: TxHash,
    ) -> impl Future<Output = Result<Option<OpTransactionReceipt>>> + Send;
}

/// Provider type with wallet signing capability for sending transactions.
///
/// Uses Ethereum network type because `send_transaction` works identically
/// for both Ethereum and Base networks. Only `RpcClient` uses the Base network
/// type since it needs `OpTransactionReceipt` for receipt handling.
pub type WalletProvider = FillProvider<
    JoinFill<JoinFill<Identity, ChainIdFiller>, WalletFiller<EthereumWallet>>,
    RootProvider<Ethereum>,
    Ethereum,
>;

/// Creates a wallet provider for the given RPC URL and wallet.
pub fn create_wallet_provider(rpc_url: Url, wallet: EthereumWallet) -> WalletProvider {
    ProviderBuilder::new()
        .disable_recommended_fillers()
        .filler(ChainIdFiller::default())
        .wallet(wallet)
        .connect_http(rpc_url)
}

/// RPC client for read-only interactions with Base nodes.
#[derive(Clone)]
pub struct RpcClient {
    provider: RootProvider<Base>,
    url: Url,
}

impl RpcClient {
    /// Creates a new RPC client.
    pub fn new(url: Url) -> Self {
        let provider = RootProvider::<Base>::new_http(url.clone());
        Self { provider, url }
    }

    /// Returns the RPC endpoint URL.
    pub const fn url(&self) -> &Url {
        &self.url
    }

    /// Fetches the chain ID from the RPC endpoint.
    #[instrument(skip(self), fields(url = %self.url))]
    pub async fn chain_id(&self) -> Result<u64> {
        self.provider.get_chain_id().await.map_err(|e| BaselineError::Rpc(e.to_string()))
    }

    /// Fetches the balance of an address at the latest block.
    #[instrument(skip(self), fields(address = %address))]
    pub async fn get_balance(&self, address: Address) -> Result<U256> {
        self.provider.get_balance(address).await.map_err(|e| BaselineError::Rpc(e.to_string()))
    }

    /// Fetches the balance of an address including pending transactions.
    #[instrument(skip(self), fields(address = %address))]
    pub async fn get_pending_balance(&self, address: Address) -> Result<U256> {
        self.provider
            .get_balance(address)
            .block_id(BlockNumberOrTag::Pending.into())
            .await
            .map_err(|e| BaselineError::Rpc(e.to_string()))
    }

    /// Fetches the nonce (transaction count) for an address.
    #[instrument(skip(self), fields(address = %address))]
    pub async fn get_nonce(&self, address: Address) -> Result<u64> {
        self.provider
            .get_transaction_count(address)
            .await
            .map_err(|e| BaselineError::Rpc(e.to_string()))
    }

    /// Fetches the transaction receipt for a given hash.
    #[instrument(skip(self), fields(tx_hash = %tx_hash))]
    pub async fn get_transaction_receipt(
        &self,
        tx_hash: TxHash,
    ) -> Result<Option<OpTransactionReceipt>> {
        self.provider
            .get_transaction_receipt(tx_hash)
            .await
            .map_err(|e| BaselineError::Rpc(e.to_string()))
    }

    /// Fetches the current gas price.
    #[instrument(skip(self))]
    pub async fn get_gas_price(&self) -> Result<u128> {
        self.provider.get_gas_price().await.map_err(|e| BaselineError::Rpc(e.to_string()))
    }

    /// Fetches transaction hashes from the pending block via `eth_getBlockByNumber("pending")`.
    #[instrument(skip(self))]
    pub async fn get_pending_block_tx_hashes(&self) -> Result<Vec<TxHash>> {
        let block = self
            .provider
            .get_block_by_number(BlockNumberOrTag::Pending)
            .hashes()
            .await
            .map_err(|e| BaselineError::Rpc(e.to_string()))?;

        Ok(block.map(|b| b.transactions.hashes().collect()).unwrap_or_default())
    }
}

impl std::fmt::Debug for RpcClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RpcClient").field("url", &self.url).finish_non_exhaustive()
    }
}

impl ReceiptProvider for RpcClient {
    async fn get_pending_block_tx_hashes(&self) -> Result<Vec<TxHash>> {
        self.get_pending_block_tx_hashes().await
    }

    async fn get_transaction_receipt(
        &self,
        tx_hash: TxHash,
    ) -> Result<Option<OpTransactionReceipt>> {
        self.get_transaction_receipt(tx_hash).await
    }
}
