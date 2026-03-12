use alloy_network::{Ethereum, EthereumWallet};
use alloy_primitives::{Address, TxHash, U256};
use alloy_provider::{
    Identity, Provider, ProviderBuilder, RootProvider,
    fillers::{FillProvider, JoinFill, WalletFiller},
};
use alloy_rpc_types::TransactionReceipt;
use tracing::instrument;
use url::Url;

use crate::utils::{BaselineError, Result};

type HttpProvider = FillProvider<
    JoinFill<
        Identity,
        JoinFill<
            alloy_provider::fillers::GasFiller,
            JoinFill<
                alloy_provider::fillers::BlobGasFiller,
                JoinFill<
                    alloy_provider::fillers::NonceFiller,
                    alloy_provider::fillers::ChainIdFiller,
                >,
            >,
        >,
    >,
    RootProvider<Ethereum>,
    Ethereum,
>;

pub(crate) type WalletProvider = FillProvider<
    JoinFill<
        JoinFill<
            Identity,
            JoinFill<
                alloy_provider::fillers::GasFiller,
                JoinFill<
                    alloy_provider::fillers::BlobGasFiller,
                    JoinFill<
                        alloy_provider::fillers::NonceFiller,
                        alloy_provider::fillers::ChainIdFiller,
                    >,
                >,
            >,
        >,
        WalletFiller<EthereumWallet>,
    >,
    RootProvider<Ethereum>,
    Ethereum,
>;

pub(crate) fn create_wallet_provider(rpc_url: Url, wallet: EthereumWallet) -> WalletProvider {
    ProviderBuilder::new().wallet(wallet).connect_http(rpc_url)
}

/// RPC client for read-only interactions with Ethereum nodes.
pub struct RpcClient {
    provider: HttpProvider,
    url: Url,
}

impl RpcClient {
    /// Creates a new RPC client.
    pub fn new(url: Url) -> Self {
        let provider = ProviderBuilder::new().connect_http(url.clone());
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
}

impl std::fmt::Debug for RpcClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RpcClient").field("url", &self.url).finish_non_exhaustive()
    }
}
