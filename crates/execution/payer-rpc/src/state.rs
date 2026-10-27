//! Node state read to check `payer_sendTransaction` transactions.

use alloy_eips::BlockId;
use alloy_primitives::{Address, U256};
use base_execution_eip8130_rpc::Eip8130EthApiOverrideServer;
use jsonrpsee::{core::async_trait, types::ErrorObjectOwned};

/// The latest-block state a transaction is checked against before it is forwarded to the payer.
#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub trait PayerChainState: Send + Sync + 'static {
    /// The next unused nonce of `sender` in the `nonce_key` channel.
    async fn nonce(&self, sender: Address, nonce_key: U256) -> Result<U256, ErrorObjectOwned>;
}

/// [`PayerChainState`] over the node's EIP-8130 `eth_getTransactionCount`.
#[derive(Debug)]
pub struct Eip8130ChainState<Api> {
    api: Api,
}

impl<Api> Eip8130ChainState<Api> {
    /// Reads through `api`, such as [`base_execution_eip8130_rpc::Eip8130EthApiExt`].
    pub const fn new(api: Api) -> Self {
        Self { api }
    }
}

#[async_trait]
impl<Api: Eip8130EthApiOverrideServer> PayerChainState for Eip8130ChainState<Api> {
    async fn nonce(&self, sender: Address, nonce_key: U256) -> Result<U256, ErrorObjectOwned> {
        self.api.get_transaction_count(sender, Some(BlockId::latest()), Some(nonce_key)).await
    }
}
