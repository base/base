//! State provider factory for Base Proofs `ExEx`.

use alloy_eips::BlockId;
use base_execution_trie::{
    BaseProofsStorage, BaseProofsStore, provider::BaseProofsStateProviderRef,
};
use jsonrpsee_types::error::ErrorObject;
use reth_provider::{BlockIdReader, ProviderError, ProviderResult, StateProvider};
use reth_rpc_api::eth::helpers::FullEthApi;
use reth_rpc_eth_types::EthApiError;

/// Creates a factory for state providers using OP Proofs external proofs storage.
#[derive(Debug)]
pub struct BaseStateProviderFactory<Eth, P> {
    eth_api: Eth,
    preimage_store: BaseProofsStorage<P>,
}

impl<Eth, P> BaseStateProviderFactory<Eth, P> {
    /// Creates a new state provider factory.
    pub const fn new(eth_api: Eth, preimage_store: BaseProofsStorage<P>) -> Self {
        Self { eth_api, preimage_store }
    }
}

impl<'a, Eth, P> BaseStateProviderFactory<Eth, P>
where
    Eth: FullEthApi + Send + Sync + 'static,
    ErrorObject<'static>: From<Eth::Error>,
    P: BaseProofsStore + Clone + 'a,
{
    /// Creates a state provider for the given block id.
    pub async fn state_provider(
        &'a self,
        block_id: Option<BlockId>,
    ) -> ProviderResult<Box<dyn StateProvider + 'a>> {
        let block_id = block_id.unwrap_or_default();
        // Check whether the distance to the block exceeds the maximum configured window.
        let block_number = self
            .eth_api
            .provider()
            .block_number_for_id(block_id)?
            .ok_or(EthApiError::HeaderNotFound(block_id))
            .map_err(ProviderError::other)?;

        let historical_provider =
            self.eth_api.state_at_block_id(block_id).await.map_err(ProviderError::other)?;

        let (Some((latest_block_number, _)), Some((earliest_block_number, _))) = (
            self.preimage_store
                .get_latest_block_number()
                .map_err(|e| ProviderError::Database(e.into()))?,
            self.preimage_store
                .get_earliest_block_number()
                .map_err(|e| ProviderError::Database(e.into()))?,
        ) else {
            // if no earliest block, db is empty, return error
            return Err(ProviderError::StateForNumberNotFound(block_number));
        };

        if block_number < earliest_block_number || block_number > latest_block_number {
            return Err(ProviderError::StateForNumberNotFound(block_number));
        }

        let external_overlay_provider = BaseProofsStateProviderRef::new(
            historical_provider,
            &self.preimage_store,
            block_number,
        );

        Ok(Box::new(external_overlay_provider))
    }
}
