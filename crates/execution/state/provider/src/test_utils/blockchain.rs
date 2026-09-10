//! Concrete provider fixtures for engine and payload service tests.

use std::sync::Arc;

use base_common_chain_config::{BaseChainSpec, ChainSpecProvider};
use base_common_types_chain::{BaseBlock, Header, RecoveredBlock, SealedHeader};
use base_execution_state_types::{
    BalStoreHandle, BlockWriter, StageCheckpoint, StageId, StaticFileSegment,
};

use super::{MockEthProvider, create_test_provider_factory_with_chain_spec};
use crate::{
    BlockchainProvider, DBProvider, DatabaseProviderFactory, InMemoryBalStore,
    StaticFileProviderFactory,
};

/// Builds and populates a temporary production blockchain provider.
#[derive(Debug)]
pub struct ProviderTestUtils;

impl ProviderTestUtils {
    /// Creates an empty database with a canonical genesis head and an in-memory BAL store.
    pub fn empty(chain_spec: Arc<BaseChainSpec>) -> BlockchainProvider {
        let header =
            SealedHeader::new(chain_spec.genesis_header().clone(), chain_spec.genesis_hash());
        let factory = create_test_provider_factory_with_chain_spec(chain_spec)
            .with_bal_store(BalStoreHandle::new(InMemoryBalStore::default()));
        BlockchainProvider::with_latest(factory, header).expect("temporary blockchain provider")
    }

    /// Materializes mock account and canonical block fixtures in a production provider.
    pub fn from_mock(mock: &MockEthProvider) -> BlockchainProvider {
        let provider = Self::empty(mock.chain_spec());
        let mut blocks: Vec<_> = mock
            .blocks
            .lock()
            .iter()
            .map(|(hash, block)| {
                let senders = vec![Default::default(); block.body.transactions.len()];
                RecoveredBlock::new(block.clone(), senders, *hash)
            })
            .collect();
        blocks.sort_by_key(|block| block.header().number);
        Self::insert_blocks(&provider, &blocks);
        let writer = provider.database_provider_rw().expect("fixture account writer");
        mock.write_accounts_to(&writer).expect("persist fixture accounts");
        writer.commit().expect("commit fixture accounts");
        provider
    }

    /// Creates a live request-serving provider from network fixture data.
    pub fn for_requests(mock: &MockEthProvider) -> BlockchainProvider {
        let mut provider = Self::empty(mock.chain_spec());
        provider.bal_store = mock.bal_store.clone();
        Self::sync_requests(&provider, mock);
        provider
    }

    /// Refreshes a request fixture while its network tasks continue serving the same provider.
    pub fn sync_requests(provider: &BlockchainProvider, mock: &MockEthProvider) {
        let writer = provider.database_provider_rw().expect("request fixture writer");
        mock.write_accounts_to(&writer).expect("request account fixtures");
        writer.commit().expect("commit request accounts");
        let mut headers: Vec<_> =
            mock.headers.lock().iter().map(|(hash, header)| (*hash, header.clone())).collect();
        headers.sort_by_key(|(_, header)| header.number);
        let blocks = mock.blocks.lock();
        let receipts = mock.receipts.lock();
        let executed: Vec<_> = headers
            .into_iter()
            .map(|(hash, header)| {
                let block = blocks
                    .get(&hash)
                    .cloned()
                    .unwrap_or_else(|| BaseBlock::new(header.clone(), Default::default()));
                let senders = vec![Default::default(); block.body.transactions.len()];
                base_execution_state_types::ExecutedBlock::new(
                    Arc::new(RecoveredBlock::new(block, senders, hash)),
                    Arc::new(base_execution_state_types::BlockExecutionOutput {
                        result: base_execution_state_types::BlockExecutionResult {
                            receipts: receipts.get(&header.number).cloned().unwrap_or_default(),
                            ..Default::default()
                        },
                        state: Default::default(),
                    }),
                    Default::default(),
                )
            })
            .collect();
        if let Some(last) = executed.last() {
            let head = last.recovered_block().clone_sealed_header();
            provider
                .canonical_in_memory_state()
                .update_chain(crate::NewCanonicalChain::Commit { new: executed });
            provider.canonical_in_memory_state().set_canonical_head(head);
        }
    }

    /// Writes an account fixture to an existing production provider.
    pub fn set_account(
        provider: &BlockchainProvider,
        address: alloy_primitives::Address,
        account: super::ExtendedAccount,
    ) {
        let mock = MockEthProvider::default();
        mock.add_account(address, account);
        let writer = provider.database_provider_rw().expect("account fixture writer");
        mock.write_accounts_to(&writer).expect("persist account fixture");
        writer.commit().expect("commit account fixture");
    }

    /// Persists blocks and advances the fixture's committed head.
    pub fn insert_blocks(provider: &BlockchainProvider, blocks: &[RecoveredBlock]) {
        let writer = provider.database_provider_rw().expect("fixture write transaction");
        let mut next = provider
            .static_file_provider()
            .get_highest_static_file_block(StaticFileSegment::Headers)
            .map_or(0, |number| number + 1);
        let mut ordered: Vec<_> = blocks.iter().collect();
        ordered.sort_by_key(|block| block.header().number);
        for block in ordered {
            // Engine fixtures often provide only the endpoints of a persisted range.
            // The real static-file store also needs headers for the intermediate heights.
            while next < block.header().number {
                let header = if next == 0 {
                    provider.chain_spec().genesis_header().clone()
                } else {
                    Header { number: next, gas_limit: 30_000_000, ..Default::default() }
                };
                let hash = header.hash_slow();
                let filler = RecoveredBlock::new(
                    BaseBlock::new(header, Default::default()),
                    Vec::new(),
                    hash,
                );
                writer.insert_block(&filler).expect("persist intermediate fixture block");
                next += 1;
            }
            if block.header().number == next {
                writer.insert_block(block).expect("persist fixture block");
                next += 1;
            }
        }
        if let Some(last) = blocks.last() {
            writer
                .save_stage_checkpoint(StageId::Finish, StageCheckpoint::new(last.header().number))
                .expect("fixture finish checkpoint");
        }
        writer.commit().expect("commit fixture blocks");
        if let Some(last) = blocks.last() {
            provider.canonical_in_memory_state().set_canonical_head(last.clone_sealed_header());
        }
    }
}
