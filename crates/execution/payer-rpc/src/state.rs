//! Node state read to check `payer_sendTransaction` transactions.

use alloy_eips::BlockId;
use alloy_primitives::{Address, U256};
use base_execution_base_tx_rpc::TxBaseEthApiOverrideServer;
use jsonrpsee::{core::async_trait, types::ErrorObjectOwned};

/// The latest-block state a transaction is checked against before it is forwarded to the payer.
#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub trait PayerChainState: Send + Sync + 'static {
    /// The next unused nonce of `sender` in the `nonce_key` channel.
    async fn nonce(&self, sender: Address, nonce_key: U256) -> Result<U256, ErrorObjectOwned>;
}

/// [`PayerChainState`] over the node's Base transaction `eth_getTransactionCount`.
#[derive(Debug)]
pub struct TxBaseChainState<Api> {
    api: Api,
}

impl<Api> TxBaseChainState<Api> {
    /// Reads through `api`, such as [`base_execution_base_tx_rpc::TxBaseEthApiExt`].
    pub const fn new(api: Api) -> Self {
        Self { api }
    }
}

#[async_trait]
impl<Api: TxBaseEthApiOverrideServer> PayerChainState for TxBaseChainState<Api> {
    async fn nonce(&self, sender: Address, nonce_key: U256) -> Result<U256, ErrorObjectOwned> {
        self.api.get_transaction_count(sender, Some(BlockId::latest()), Some(nonce_key)).await
    }
}
