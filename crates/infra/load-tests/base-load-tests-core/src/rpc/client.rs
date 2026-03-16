use std::future::Future;

use alloy_network::{Ethereum, EthereumWallet};
use alloy_primitives::{Address, TxHash, U256};
use alloy_provider::{
    Identity, Provider, ProviderBuilder, RootProvider,
    fillers::{ChainIdFiller, FillProvider, JoinFill, WalletFiller},
};
use alloy_rpc_types::{BlockId, BlockNumberOrTag, TransactionReceipt};
use tracing::instrument;
use url::Url;

use crate::utils::{BaselineError, Result};

/// Provider trait for fetching transaction receipts and block data.
///
/// This trait abstracts the RPC calls needed by the confirmer, enabling
/// mock implementations for testing.
pub trait ReceiptProvider: Send + Sync {
    /// Fetches the current block number.
    fn get_block_number(&self) -> impl Future<Output = Result<u64>> + Send;

    /// Fetches all transaction receipts for a given block.
    fn get_block_receipts(
        &self,
        block_number: u64,
    ) -> impl Future<Output = Result<Option<Vec<TransactionReceipt>>>> + Send;

    /// Fetches the transaction receipt for a given hash.
    fn get_transaction_receipt(
        &self,
        tx_hash: TxHash,
    ) -> impl Future<Output = Result<Option<TransactionReceipt>>> + Send;
}

type HttpProvider = RootProvider<Ethereum>;

/// Provider type with wallet signing capability for sending transactions.
///
/// Uses only `ChainIdFiller` and `WalletFiller`. `GasFiller` and `NonceFiller`
/// are intentionally omitted since nonce and gas are explicitly managed by
/// the load runner to avoid redundant RPC calls.
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

/// RPC client for read-only interactions with Ethereum nodes.
pub struct RpcClient {
    provider: HttpProvider,
    url: Url,
}

impl RpcClient {
    /// Creates a new RPC client.
    pub fn new(url: Url) -> Self {
        let provider = RootProvider::new_http(url.clone());
        Self { provider, url }
    }

    /// Returns the RPC endpoint URL.
    pub const fn url(&self) -> &Url {
        &self.url
    }

    /// Returns a reference to the underlying provider.
    pub const fn provider(&self) -> &HttpProvider {
        &self.provider
    }

    /// Fetches the chain ID from the RPC endpoint.
    #[instrument(skip(self), fields(url = %self.url))]
    pub async fn chain_id(&self) -> Result<u64> {
        self.provider.get_chain_id().await.map_err(|e| BaselineError::Rpc(e.to_string()))
    }

    /// Fetches the balance of an address.
    #[instrument(skip(self), fields(address = %address))]
    pub async fn get_balance(&self, address: Address) -> Result<U256> {
        self.provider.get_balance(address).await.map_err(|e| BaselineError::Rpc(e.to_string()))
    }

    /// Fetches the nonce (transaction count) for an address.
    #[instrument(skip(self), fields(address = %address))]
    pub async fn get_nonce(&self, address: Address) -> Result<u64> {
        self.provider
            .get_transaction_count(address)
            .await
            .map_err(|e| BaselineError::Rpc(e.to_string()))
    }

    /// Fetches the current block number.
    #[instrument(skip(self))]
    pub async fn get_block_number(&self) -> Result<u64> {
        self.provider.get_block_number().await.map_err(|e| BaselineError::Rpc(e.to_string()))
    }

    /// Fetches the transaction receipt for a given hash.
    #[instrument(skip(self), fields(tx_hash = %tx_hash))]
    pub async fn get_transaction_receipt(
        &self,
        tx_hash: TxHash,
    ) -> Result<Option<TransactionReceipt>> {
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

    /// Fetches all transaction receipts for a given block number.
    #[instrument(skip(self), fields(block_number = block_number))]
    pub async fn get_block_receipts(
        &self,
        block_number: u64,
    ) -> Result<Option<Vec<TransactionReceipt>>> {
        self.provider
            .get_block_receipts(BlockId::Number(BlockNumberOrTag::Number(block_number)))
            .await
            .map_err(|e| BaselineError::Rpc(e.to_string()))
    }
}

impl std::fmt::Debug for RpcClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RpcClient").field("url", &self.url).finish_non_exhaustive()
    }
}

impl ReceiptProvider for RpcClient {
    async fn get_block_number(&self) -> Result<u64> {
        self.get_block_number().await
    }

    async fn get_block_receipts(
        &self,
        block_number: u64,
    ) -> Result<Option<Vec<TransactionReceipt>>> {
        self.get_block_receipts(block_number).await
    }

    async fn get_transaction_receipt(&self, tx_hash: TxHash) -> Result<Option<TransactionReceipt>> {
        self.get_transaction_receipt(tx_hash).await
    }
}
