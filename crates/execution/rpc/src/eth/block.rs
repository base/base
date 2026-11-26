//! Loads and formats Base block RPC response.

use std::sync::Arc;

use alloy_eips::{BlockId, eip7928::bal::DecodedBal};
use base_common_consensus::BaseTransaction;
use base_common_rpc_types::BaseHeaderResponse;
use reth_node_api::BlockBody;
use reth_primitives_traits::{AlloyBlockHeader, NodePrimitives, RecoveredBlock};
use reth_rpc_eth_api::{
    EthApiTypes, FromEthApiError, FromEvmError, FullEthApiTypes, RpcBlock, RpcConvert, RpcHeader,
    RpcNodeCoreExt, RpcTypes,
    helpers::{EthBlocks, LoadBlock, LoadPendingBlock},
};
use reth_storage_api::{BlockIdReader, BlockReader, ProviderBlock};
use revm::state::bal::Bal as RevmBal;

use crate::{BaseEthApi, BaseEthApiError, eth::RpcNodeCore};

impl<N, Rpc> EthBlocks for BaseEthApi<N, Rpc>
where
    N: RpcNodeCore,
    BaseEthApiError: FromEvmError<N::Evm>,
    Rpc: RpcConvert<Primitives = N::Primitives, Error = BaseEthApiError>,
    <Self as EthApiTypes>::NetworkTypes: RpcTypes<Header = BaseHeaderResponse>,
    <Self::Primitives as NodePrimitives>::SignedTx: BaseTransaction,
{
    async fn rpc_block_header(
        &self,
        block_id: BlockId,
    ) -> Result<Option<RpcHeader<Self::NetworkTypes>>, Self::Error>
    where
        Self: FullEthApiTypes,
    {
        let Some(block) = self.recovered_block(block_id).await? else { return Ok(None) };
        let timestamp_ms = self.base_time_cache().insert_from_transactions(
            block.hash(),
            block.number(),
            block.timestamp(),
            block.body().transactions(),
        );
        let mut header = self
            .converter()
            .convert_header(block.clone_sealed_header(), Some(block.rlp_length()))?;
        header.timestamp_ms = timestamp_ms;
        Ok(Some(header))
    }

    async fn rpc_block(
        &self,
        block_id: BlockId,
        full: bool,
    ) -> Result<Option<RpcBlock<Self::NetworkTypes>>, Self::Error>
    where
        Self: FullEthApiTypes,
    {
        let Some(block) = self.recovered_block(block_id).await? else { return Ok(None) };
        let timestamp_ms = self.base_time_cache().insert_from_transactions(
            block.hash(),
            block.number(),
            block.timestamp(),
            block.body().transactions(),
        );
        let mut block = block.clone_into_rpc_block(
            full.into(),
            |tx, tx_info| self.converter().fill(tx, tx_info),
            |header, size| self.converter().convert_header(header, Some(size)),
        )?;
        block.header.timestamp_ms = timestamp_ms;
        Ok(Some(block))
    }
}

impl<N, Rpc> LoadBlock for BaseEthApi<N, Rpc>
where
    N: RpcNodeCore,
    BaseEthApiError: FromEvmError<N::Evm>,
    Rpc: RpcConvert<Primitives = N::Primitives, Error = BaseEthApiError>,
{
    /// reth's default; a miss or prune refusal for a canonical block is then asked of the
    /// [`PrunedHistory`](crate::PrunedHistory) hook, when one is installed.
    async fn recovered_block(
        &self,
        block_id: BlockId,
    ) -> Result<Option<Arc<RecoveredBlock<ProviderBlock<Self::Provider>>>>, Self::Error> {
        let local = self.local_recovered_block(block_id).await;
        let Some(hook) = self.block_fallback(&local) else { return local };
        let Some((number, hash)) = self.canonical_block(block_id)? else { return local };
        Self::pruned_block(&hook, number, hash).await?.map_or(local, |block| Ok(Some(block)))
    }

    /// reth's default; a miss or prune refusal for a canonical block is then asked of the
    /// [`PrunedHistory`](crate::PrunedHistory) hook (an archived block has no cached BAL).
    async fn recovered_block_and_maybe_bal(
        &self,
        block_id: BlockId,
    ) -> Result<
        Option<(Arc<RecoveredBlock<ProviderBlock<Self::Provider>>>, Option<CachedBal>)>,
        Self::Error,
    > {
        let local = if block_id.is_pending() {
            self.recovered_block(block_id).await.map(|block| block.map(|block| (block, None)))
        } else {
            match self.provider().block_hash_for_id(block_id).map_err(Self::Error::from_eth_err)? {
                Some(block_hash) => self
                    .cache()
                    .get_recovered_block_and_maybe_bal(block_hash)
                    .await
                    .map_err(Self::Error::from_eth_err),
                None => Ok(None),
            }
        };
        let Some(hook) = self.block_fallback(&local) else { return local };
        let Some((number, hash)) = self.canonical_block(block_id)? else { return local };
        Self::pruned_block(&hook, number, hash)
            .await?
            .map_or(local, |block| Ok(Some((block, None))))
    }
}

/// A block access list as the state cache returns it.
pub type CachedBal = Arc<DecodedBal<Arc<RevmBal>>>;

impl<N, Rpc> BaseEthApi<N, Rpc>
where
    N: RpcNodeCore,
    BaseEthApiError: FromEvmError<N::Evm>,
    Rpc: RpcConvert<Primitives = N::Primitives, Error = BaseEthApiError>,
{
    /// reth's default `LoadBlock::recovered_block`, mirrored exactly (an override cannot call
    /// the default it replaces).
    pub async fn local_recovered_block(
        &self,
        block_id: BlockId,
    ) -> Result<Option<Arc<RecoveredBlock<ProviderBlock<N::Provider>>>>, BaseEthApiError> {
        if block_id.is_pending() {
            if self.pending_block_kind().is_none() {
                return Ok(None);
            }

            // Pending block can be fetched directly without need for caching
            if let Some(pending_block) =
                self.provider().pending_block().map_err(BaseEthApiError::from_eth_err)?
            {
                return Ok(Some(pending_block));
            }

            // If no pending block from provider, try to get local pending block
            return match self.local_pending_block().await? {
                Some(pending) => Ok(Some(pending.block)),
                None => Ok(None),
            };
        }

        let block_hash = match self
            .provider()
            .block_hash_for_id(block_id)
            .map_err(BaseEthApiError::from_eth_err)?
        {
            Some(block_hash) => block_hash,
            None => return Ok(None),
        };

        self.cache().get_recovered_block(block_hash).await.map_err(BaseEthApiError::from_eth_err)
    }
}
