//! Public read-only RPC for native BLS block authorization.

use std::sync::Arc;

use alloy_primitives::{B256, Bytes, FixedBytes};
use base_common_consensus::{BLS_TRANSFER_TX_TYPE_ID, BlsConstants};
use base_common_evm::{BlsChunkError, BlsChunkVerifier, BlsDatabaseState};
use base_execution_evm::{BlsStoredWitness, BlsWitnessStore};
use jsonrpsee::proc_macros::rpc;
use jsonrpsee_core::{RpcResult, async_trait};
use jsonrpsee_types::{
    ErrorObjectOwned,
    error::{INTERNAL_ERROR_CODE, INVALID_PARAMS_CODE},
};
use reth_chainspec::{ChainSpecProvider, EthChainSpec};
use reth_revm::database::StateProviderDatabase;
use reth_storage_api::StateProviderFactory;

/// Native BLS block authorization queries for batchers and derivation.
#[cfg_attr(not(feature = "client"), rpc(server, namespace = "base"))]
#[cfg_attr(feature = "client", rpc(server, client, namespace = "base"))]
pub trait BaseBlsApi {
    /// Returns the aggregate BLS witness recorded for the block, or `None` when the node has
    /// none: the block carries no native BLS transfers, was authorized by derivation (which has
    /// no per-block signature), or is unknown.
    #[method(name = "blsBlockAggregate")]
    async fn bls_block_aggregate(&self, block_hash: B256) -> RpcResult<Option<FixedBytes<48>>>;

    /// Verifies the native BLS authorization of a chunk of consecutive blocks' EIP-2718
    /// transactions against the state at `parent_hash` and the chunk `aggregate`.
    ///
    /// Returns `false` only when the chunk's authorization is invalid. Out-of-bounds input, an
    /// unsupported chain, and state access failures are errors.
    #[method(name = "verifyBlsChunk")]
    async fn verify_bls_chunk(
        &self,
        parent_hash: B256,
        blocks: Vec<Vec<Bytes>>,
        aggregate: Option<FixedBytes<48>>,
    ) -> RpcResult<bool>;
}

/// [`BaseBlsApiServer`] backed by the node's state and durable BLS witness store.
#[derive(Debug, Clone)]
pub struct BaseBlsApi<Provider> {
    provider: Provider,
    store: Option<Arc<BlsWitnessStore>>,
}

impl<Provider> BaseBlsApi<Provider> {
    /// Creates the API over `provider` and the node's witness store, if it keeps one.
    pub const fn new(provider: Provider, store: Option<Arc<BlsWitnessStore>>) -> Self {
        Self { provider, store }
    }

    /// Maps a node-side failure to an internal RPC error.
    pub fn internal_error(error: impl core::fmt::Display) -> ErrorObjectOwned {
        ErrorObjectOwned::owned(INTERNAL_ERROR_CODE, error.to_string(), None::<()>)
    }

    /// Maps out-of-bounds or unsupported input to an invalid-params RPC error.
    pub fn invalid_params(error: impl core::fmt::Display) -> ErrorObjectOwned {
        ErrorObjectOwned::owned(INVALID_PARAMS_CODE, error.to_string(), None::<()>)
    }
}

#[async_trait]
impl<Provider> BaseBlsApiServer for BaseBlsApi<Provider>
where
    Provider: StateProviderFactory + ChainSpecProvider + Clone + 'static,
{
    async fn bls_block_aggregate(&self, block_hash: B256) -> RpcResult<Option<FixedBytes<48>>> {
        let Some(store) = &self.store else { return Ok(None) };
        match store.get(block_hash).map_err(Self::internal_error)? {
            Some(BlsStoredWitness::Aggregate(aggregate)) => Ok(Some(aggregate)),
            Some(BlsStoredWitness::Derived) | None => Ok(None),
        }
    }

    async fn verify_bls_chunk(
        &self,
        parent_hash: B256,
        blocks: Vec<Vec<Bytes>>,
        aggregate: Option<FixedBytes<48>>,
    ) -> RpcResult<bool> {
        let chain_id = self.provider.chain_spec().chain().id();
        if chain_id != BlsConstants::CHAIN_ID {
            return Err(Self::invalid_params(format!(
                "native BLS transfers are only supported on chain {}",
                BlsConstants::CHAIN_ID
            )));
        }
        // The verifier's cheap bounds, applied before opening state: block and transfer counts
        // (transfers are counted by type byte) and the aggregate's presence.
        if blocks.len() > BlsChunkVerifier::MAX_BLOCKS {
            return Err(Self::invalid_params(
                BlsChunkError::<core::convert::Infallible>::TooManyBlocks { blocks: blocks.len() },
            ));
        }
        let transfers = blocks
            .iter()
            .flatten()
            .filter(|tx| tx.first() == Some(&BLS_TRANSFER_TX_TYPE_ID))
            .count();
        if transfers > BlsChunkVerifier::MAX_TRANSFERS {
            return Err(Self::invalid_params(
                BlsChunkError::<core::convert::Infallible>::TooManyTransfers { transfers },
            ));
        }
        if aggregate.is_some() != (transfers > 0) {
            return Ok(false);
        }
        let provider = self.provider.clone();
        // Decoding and the multi-pairing are CPU bound; keep them off the RPC executor.
        let result = tokio::task::spawn_blocking(move || {
            let state = provider.state_by_block_hash(parent_hash)?;
            let mut db = StateProviderDatabase::new(state);
            Ok::<_, reth_storage_api::errors::ProviderError>(BlsChunkVerifier::verify(
                &mut BlsDatabaseState(&mut db),
                chain_id,
                &blocks,
                aggregate,
            ))
        })
        .await
        .map_err(Self::internal_error)?
        .map_err(Self::internal_error)?;
        match result {
            Ok(()) => Ok(true),
            Err(BlsChunkError::Database(error)) => Err(Self::internal_error(error)),
            Err(
                error @ (BlsChunkError::UnsupportedChain { .. }
                | BlsChunkError::TooManyBlocks { .. }
                | BlsChunkError::TooManyTransfers { .. }),
            ) => Err(Self::invalid_params(error)),
            Err(_) => Ok(false),
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_consensus::Sealable;
    use alloy_eips::eip2718::Encodable2718;
    use alloy_primitives::{Address, U256};
    use base_common_consensus::{BasePrimitives, BaseTxEnvelope, TxBlsTransfer};
    use base_common_evm::{BlsRegistry, test_utils::BlsTestKey};
    use base_execution_chainspec::{BaseChainSpec, BaseChainSpecBuilder};
    use reth_chainspec::Chain;
    use reth_provider::test_utils::{ExtendedAccount, MockEthProvider};

    use super::*;

    type Provider = MockEthProvider<BasePrimitives, Arc<BaseChainSpec>>;

    fn provider_on(chain_id: u64) -> Provider {
        let chain_spec =
            Arc::new(BaseChainSpecBuilder::base_mainnet().chain(Chain::from_id(chain_id)).build());
        MockEthProvider::<BasePrimitives>::new().with_chain_spec(chain_spec)
    }

    fn register(provider: &Provider, owner: Address, key: &BlsTestKey) {
        let storage = BlsRegistry::key_slots(owner)
            .into_iter()
            .zip(BlsRegistry::key_words(&key.public_key().0));
        provider.add_account(
            BlsConstants::REGISTRY_ADDRESS,
            ExtendedAccount::new(BlsRegistry::ACCOUNT_NONCE, U256::ZERO).extend_storage(storage),
        );
    }

    fn transfer(sender: Address) -> TxBlsTransfer {
        TxBlsTransfer {
            chain_id: BlsConstants::CHAIN_ID,
            sender,
            nonce: 0,
            recipient: Address::repeat_byte(0x22),
            value: U256::from(1u64),
            max_priority_fee_per_gas: 1,
            max_fee_per_gas: 2,
        }
    }

    /// The chunk check answers `true` for an honest aggregate and `false` for any invalid
    /// authorization, including a missing or unexpected aggregate decided before state access,
    /// while unsupported chains and out-of-bounds input are errors.
    #[tokio::test]
    async fn verify_bls_chunk_distinguishes_invalid_authorization_from_bad_requests() {
        let key = BlsTestKey::from_seed(1);
        let sender = Address::repeat_byte(0x11);
        let provider = provider_on(BlsConstants::CHAIN_ID);
        register(&provider, sender, &key);
        let api = BaseBlsApi::new(provider, None);
        let tx = transfer(sender);
        let raw = BaseTxEnvelope::from(tx.seal_slow()).encoded_2718();
        let blocks = vec![vec![Bytes::from(raw)], vec![]];
        let signature = key.sign(tx.signing_digest());
        let verify = |blocks: Vec<Vec<Bytes>>, aggregate| {
            api.verify_bls_chunk(B256::ZERO, blocks, aggregate)
        };

        assert!(verify(blocks.clone(), Some(signature)).await.unwrap());
        let forged = BlsTestKey::from_seed(2).sign(tx.signing_digest());
        assert!(!verify(blocks.clone(), Some(forged)).await.unwrap());
        assert!(!verify(blocks.clone(), None).await.unwrap());
        assert!(!verify(vec![vec![]], Some(signature)).await.unwrap());
        assert!(verify(vec![vec![]], None).await.unwrap(), "a chunk without transfers needs none");

        let too_many_blocks = vec![vec![]; BlsChunkVerifier::MAX_BLOCKS + 1];
        assert_eq!(verify(too_many_blocks, None).await.unwrap_err().code(), INVALID_PARAMS_CODE);

        let mainnet = BaseBlsApi::new(provider_on(8_453), None);
        assert_eq!(
            mainnet.verify_bls_chunk(B256::ZERO, blocks, Some(signature)).await.unwrap_err().code(),
            INVALID_PARAMS_CODE
        );
    }

    /// Only a recorded aggregate is returned; a derived block has no per-block signature.
    #[tokio::test]
    async fn bls_block_aggregate_returns_only_recorded_aggregates() {
        let dir = std::env::temp_dir().join(format!(
            "bls-rpc-store-{}-{}",
            std::process::id(),
            B256::random()
        ));
        let store = Arc::new(BlsWitnessStore::open(dir.clone()).unwrap());
        let aggregate = FixedBytes::<48>::repeat_byte(0xab);
        store.put(B256::repeat_byte(1), BlsStoredWitness::Aggregate(aggregate)).unwrap();
        store.put(B256::repeat_byte(2), BlsStoredWitness::Derived).unwrap();
        let api = BaseBlsApi::new(provider_on(BlsConstants::CHAIN_ID), Some(store));

        assert_eq!(api.bls_block_aggregate(B256::repeat_byte(1)).await.unwrap(), Some(aggregate));
        assert_eq!(api.bls_block_aggregate(B256::repeat_byte(2)).await.unwrap(), None);
        assert_eq!(api.bls_block_aggregate(B256::repeat_byte(3)).await.unwrap(), None);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
