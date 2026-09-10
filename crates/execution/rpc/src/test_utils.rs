//! RPC fixtures backed by the production Base provider, transaction pool, and network handle.

use std::sync::Arc;

use base_common_runtime::Runtime;
use base_common_types_chain::{BaseBlock, RecoveredBlock};
use base_execution_evm_blocks::BaseEvmConfig;
use base_execution_network_service::NetworkHandle;
use base_execution_state_provider::{
    ChainSpecProvider, NewCanonicalChain, StaticFileProviderFactory,
    providers::BlockchainProvider,
    test_utils::{MockEthProvider, create_test_provider_factory_with_chain_spec},
};
use base_execution_state_types::{BlockExecutionOutput, BlockExecutionResult, ExecutedBlock};
use base_execution_txpool::{
    BaseOrdering, BaseTransactionPool, BaseTransactionValidatorBuilder, DiskFileBlobStore, Pool,
};

use crate::{BaseRpcContext, EthApiBuilder};

/// Production Base pool used by RPC tests.
pub type TestPool = BaseTransactionPool;

/// Constructs concrete Base fixtures for RPC tests.
#[derive(Debug)]
pub struct RpcTestUtils;

impl RpcTestUtils {
    /// Materializes fixture accounts and blocks in a real Base provider.
    pub fn provider(mock: MockEthProvider) -> BlockchainProvider {
        let factory = create_test_provider_factory_with_chain_spec(mock.chain_spec());
        base_execution_state_operations::init::init_genesis(&factory)
            .expect("initialize RPC fixture genesis");
        let writer = factory.provider_rw().expect("fixture writer");
        mock.write_accounts_to(&writer).expect("fixture accounts");
        writer.commit().expect("commit fixture accounts");
        let provider = BlockchainProvider::new(factory).expect("fixture provider");
        let mut headers: Vec<_> =
            mock.headers.lock().iter().map(|(hash, header)| (*hash, header.clone())).collect();
        headers.sort_by_key(|(_, header)| header.number);
        let mut parent_hash = provider.chain_spec().genesis_hash();
        let mut executed = Vec::new();
        for (hash, mut header) in headers {
            header.parent_hash = parent_hash;
            let mut block = mock
                .blocks
                .lock()
                .get(&hash)
                .cloned()
                .unwrap_or_else(|| BaseBlock::new(header.clone(), Default::default()));
            block.header = header.clone();
            let senders = vec![Default::default(); block.body.transactions.len()];
            let receipts = mock.receipts.lock().get(&header.number).cloned().unwrap_or_default();
            executed.push(ExecutedBlock::new(
                Arc::new(RecoveredBlock::new(block, senders, hash)),
                Arc::new(BlockExecutionOutput {
                    result: BlockExecutionResult { receipts, ..Default::default() },
                    state: Default::default(),
                }),
                Default::default(),
            ));
            parent_hash = hash;
        }
        if let Some(last) = executed.last() {
            let head = last.recovered_block().clone_sealed_header();
            let state = provider.canonical_in_memory_state();
            state.update_chain(NewCanonicalChain::Commit { new: executed });
            state.set_canonical_head(head);
        }
        provider
    }

    /// Creates a pool for RPC lifecycle tests, which supply pre-recovered transaction fixtures.
    pub fn pool_without_balance_checks(mock: MockEthProvider) -> TestPool {
        let provider = Self::provider(mock);
        let evm_config = BaseEvmConfig::new(provider.chain_spec());
        let store = DiskFileBlobStore::open(
            provider.static_file_provider().directory().join("rpc_blobs"),
            Default::default(),
        )
        .unwrap();
        let validator = BaseTransactionValidatorBuilder::new(provider, evm_config)
            .disable_balance_check()
            .build()
            .require_l1_data_gas_fee(false);
        let validator = base_execution_txpool::TransactionValidationTaskExecutor::spawn(
            validator,
            &Runtime::test(),
            0,
        );
        let ordering = BaseOrdering::default();
        BaseTransactionPool::new(
            Pool::new(validator, ordering.clone(), store, Default::default()),
            ordering,
        )
    }

    /// Creates the production pool and a detached network handle for the supplied fixtures.
    pub fn context(mock: MockEthProvider) -> BaseRpcContext {
        let provider = Self::provider(mock);
        let evm_config = BaseEvmConfig::new(provider.chain_spec());
        let store = DiskFileBlobStore::open(
            provider.static_file_provider().directory().join("rpc_blobs"),
            Default::default(),
        )
        .expect("fixture blob store");
        let ordering = BaseOrdering::default();
        let validator = BaseTransactionValidatorBuilder::new(provider.clone(), evm_config.clone())
            .build_with_tasks(Runtime::test());
        let pool = BaseTransactionPool::new(
            Pool::new(validator, ordering.clone(), store, Default::default()),
            ordering,
        );
        let network = NetworkHandle::test(provider.chain_spec().chain_id());
        BaseRpcContext { provider, pool, network, evm_config }
    }

    /// Builds RPC handlers from a fixture snapshot using production components.
    pub fn api_builder(mock: MockEthProvider) -> EthApiBuilder {
        EthApiBuilder::new_with_components(Self::context(mock))
    }
}
