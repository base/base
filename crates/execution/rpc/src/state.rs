//! State provider factory for Base Proofs `ExEx`.

use alloy_eips::BlockId;
use alloy_primitives::{B256, BlockNumber};
use base_execution_trie::{
    BaseProofsStorage, BaseProofsStore, provider::BaseProofsStateProviderRef,
};
use jsonrpsee_types::error::ErrorObject;
use reth_provider::{BlockIdReader, BlockNumReader, ProviderError, ProviderResult, StateProvider};
use reth_rpc_api::eth::helpers::FullEthApi;
use reth_rpc_eth_types::EthApiError;

/// Creates a factory for state providers using external proofs storage.
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

    /// Whether the proofs store's latest block is on `provider`'s canonical chain.
    ///
    /// After a reorg the store keeps the abandoned branch until the proofs `ExEx` rewrites it, and
    /// its reads for any block above the fork point would come from that branch. A reorg to a
    /// lower head leaves the abandoned blocks' hashes in the database until they are unwound, so
    /// the store's latest block must also not be above the canonical head.
    pub fn is_store_head_canonical(
        provider: &impl BlockNumReader,
        number: BlockNumber,
        hash: B256,
    ) -> ProviderResult<bool> {
        Ok(number <= provider.best_block_number()? && provider.block_hash(number)? == Some(hash))
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
        let block_number = self
            .eth_api
            .provider()
            .block_number_for_id(block_id)?
            .ok_or(EthApiError::HeaderNotFound(block_id))
            .map_err(ProviderError::other)?;

        let historical_provider =
            self.eth_api.state_at_block_id(block_id).await.map_err(ProviderError::other)?;

        let (Some((latest_block_number, latest_block_hash)), Some((earliest_block_number, _))) = (
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

        // Serve nothing from a reorged branch until the store is canonical again.
        if !Self::is_store_head_canonical(
            self.eth_api.provider(),
            latest_block_number,
            latest_block_hash,
        )? {
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

#[cfg(test)]
mod tests {
    //! The canonical-head check runs against a hand-rolled chain view: reth's `MockEthProvider`
    //! takes its best block from its highest header, so it cannot hold a stale hash above the
    //! canonical head, which is the state a reorg to a lower head leaves behind.

    use std::collections::HashMap;

    use reth_chainspec::ChainInfo;
    use reth_provider::BlockHashReader;

    use super::*;

    /// Canonical head number plus the hash the database returns for each block number.
    struct Chain {
        best: BlockNumber,
        hashes: HashMap<BlockNumber, B256>,
    }

    impl BlockHashReader for Chain {
        fn block_hash(&self, number: BlockNumber) -> ProviderResult<Option<B256>> {
            Ok(self.hashes.get(&number).copied())
        }

        fn canonical_hashes_range(
            &self,
            start: BlockNumber,
            end: BlockNumber,
        ) -> ProviderResult<Vec<B256>> {
            Ok((start..end).filter_map(|n| self.hashes.get(&n).copied()).collect())
        }
    }

    impl BlockNumReader for Chain {
        fn chain_info(&self) -> ProviderResult<ChainInfo> {
            Ok(ChainInfo { best_hash: self.hashes[&self.best], best_number: self.best })
        }

        fn best_block_number(&self) -> ProviderResult<BlockNumber> {
            Ok(self.best)
        }

        fn last_block_number(&self) -> ProviderResult<BlockNumber> {
            Ok(self.best)
        }

        fn block_number(&self, hash: B256) -> ProviderResult<Option<BlockNumber>> {
            Ok(self.hashes.iter().find_map(|(n, h)| (*h == hash).then_some(*n)))
        }
    }

    fn is_canonical(chain: &Chain, number: BlockNumber, hash: B256) -> bool {
        BaseStateProviderFactory::<(), ()>::is_store_head_canonical(chain, number, hash).unwrap()
    }

    #[test]
    fn store_head_on_the_canonical_chain_is_served() {
        let chain = Chain {
            best: 11,
            hashes: HashMap::from([(10, B256::with_last_byte(10)), (11, B256::with_last_byte(11))]),
        };
        assert!(is_canonical(&chain, 10, B256::with_last_byte(10)));
        assert!(is_canonical(&chain, 11, B256::with_last_byte(11)));
    }

    #[test]
    fn store_head_replaced_at_the_same_height_is_refused() {
        let chain = Chain { best: 10, hashes: HashMap::from([(10, B256::with_last_byte(0xb))]) };
        assert!(!is_canonical(&chain, 10, B256::with_last_byte(0xa)));
    }

    #[test]
    fn store_head_above_a_lower_canonical_head_is_refused() {
        // The reorg dropped block 10, but its hash is still in the database until it is unwound.
        let chain = Chain {
            best: 9,
            hashes: HashMap::from([(9, B256::with_last_byte(9)), (10, B256::with_last_byte(10))]),
        };
        assert!(!is_canonical(&chain, 10, B256::with_last_byte(10)));
    }
}
