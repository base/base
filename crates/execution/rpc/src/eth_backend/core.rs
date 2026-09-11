//! Implementation of the [`jsonrpsee`] generated [`EthApiServer`](BaseEthApi) trait
//! Handles RPC requests for the `eth_` namespace.

use std::{sync::Arc, time::Duration};

use alloy_eips::BlockNumberOrTag;
use alloy_primitives::{Bytes, U256};
use alloy_rpc_client::RpcClient;
use base_common_runtime::{
    Runtime,
    pool::{BlockingTaskGuard, BlockingTaskPool},
};
use base_common_types_chain::BlockHeader;
use base_common_types_rpc::PendingBlockKind;
use base_execution_evm_blocks::BaseEvmConfig;
use base_execution_state_provider::providers::BlockchainProvider;
use base_execution_state_types::BlockReaderIdExt;
use base_execution_txpool::{
    AddedTransactionOutcome, BaseTransactionPool, BatchTxProcessor, BatchTxRequest,
};
use tokio::sync::{Mutex, Semaphore, broadcast, mpsc};

use crate::{
    BaseRpcContext, BaseRpcConverter, BaseTimeCache, EthApiError, EthStateCache, FeeHistoryCache,
    GasCap, GasPriceOracle, PendingBlock, SequencerClient, SignersForRpc,
};

const DEFAULT_BROADCAST_CAPACITY: usize = 2000;

/// Container type `BaseEthApi`
#[expect(missing_debug_implementations)]
pub struct BaseEthApiInner {
    /// Configured sequencer transaction forwarder.
    pub sequencer_client: Option<SequencerClient>,
    /// Minimum priority fee for Base gas suggestions.
    pub min_suggested_priority_fee: U256,
    /// Validated BaseTime timestamp cache.
    pub base_time: BaseTimeCache,
    /// The components of the node.
    components: BaseRpcContext,
    /// All configured Signers
    signers: SignersForRpc,
    /// The async cache frontend for eth related data
    eth_cache: EthStateCache,
    /// The async gas oracle frontend for gas price suggestions
    gas_oracle: GasPriceOracle<BlockchainProvider>,
    /// Maximum gas limit for `eth_call` and call tracing RPC methods.
    gas_cap: u64,
    /// Maximum number of blocks for `eth_simulateV1`.
    max_simulate_blocks: u64,
    /// Whether to compute state roots for `eth_simulateV1`.
    compute_state_root_for_eth_simulate: bool,
    /// The maximum number of blocks into the past for generating state proofs.
    eth_proof_window: u64,
    /// The block number at which the node started
    starting_block: U256,
    /// The type that can spawn tasks which would otherwise block.
    task_spawner: Runtime,
    /// Cached pending block if any
    pending_block: Mutex<Option<PendingBlock>>,
    /// A pool dedicated to CPU heavy blocking tasks.
    blocking_task_pool: BlockingTaskPool,
    /// Cache for block fees history
    fee_history_cache: FeeHistoryCache,

    /// Guard for getproof calls
    blocking_task_guard: BlockingTaskGuard,

    /// Semaphore to limit concurrent blocking IO requests (`eth_call`, `eth_estimateGas`, etc.)
    blocking_io_request_semaphore: Arc<Semaphore>,

    /// Transaction broadcast channel
    raw_tx_sender: broadcast::Sender<Bytes>,

    /// Raw transaction forwarder
    raw_tx_forwarder: Option<RpcClient>,

    /// Converter for RPC types.
    converter: BaseRpcConverter,

    /// Builder for pending block environment.

    /// Transaction batch sender for batching tx insertions
    tx_batch_sender: mpsc::UnboundedSender<BatchTxRequest>,

    /// Configuration for pending block construction.
    pending_block_kind: PendingBlockKind,

    /// Timeout duration for `send_raw_transaction_sync` RPC method.
    send_raw_transaction_sync_timeout: Duration,

    /// Maximum memory the EVM can allocate per RPC request.
    evm_memory_limit: u64,
}

impl BaseEthApiInner {
    /// Creates a new, shareable instance using the default tokio task spawner.
    #[expect(clippy::too_many_arguments)]
    pub fn new(
        components: BaseRpcContext,
        eth_cache: EthStateCache,
        gas_oracle: GasPriceOracle<BlockchainProvider>,
        gas_cap: impl Into<GasCap>,
        max_simulate_blocks: u64,
        compute_state_root_for_eth_simulate: bool,
        eth_proof_window: u64,
        blocking_task_pool: BlockingTaskPool,
        fee_history_cache: FeeHistoryCache,
        task_spawner: Runtime,
        proof_permits: usize,
        converter: BaseRpcConverter,

        max_batch_size: usize,
        max_blocking_io_requests: usize,
        pending_block_kind: PendingBlockKind,
        raw_tx_forwarder: Option<RpcClient>,
        send_raw_transaction_sync_timeout: Duration,
        evm_memory_limit: u64,
    ) -> Self {
        let signers = parking_lot::RwLock::new(Default::default());
        // get the block number of the latest block
        let starting_block = U256::from(
            components
                .provider
                .header_by_number_or_tag(BlockNumberOrTag::Latest)
                .ok()
                .flatten()
                .map(|header| header.number())
                .unwrap_or_default(),
        );

        let (raw_tx_sender, _) = broadcast::channel(DEFAULT_BROADCAST_CAPACITY);

        // Create tx pool insertion batcher
        let (processor, tx_batch_sender) =
            BatchTxProcessor::new(components.pool.clone(), max_batch_size);
        task_spawner.spawn_critical_task("tx-batcher", processor);

        Self {
            sequencer_client: None,
            min_suggested_priority_fee: U256::from(1_000_000),
            base_time: converter.base_time.clone(),
            components,
            signers,
            eth_cache,
            gas_oracle,
            gas_cap: gas_cap.into().into(),
            max_simulate_blocks,
            compute_state_root_for_eth_simulate,
            eth_proof_window,
            starting_block,
            task_spawner,
            pending_block: Default::default(),
            blocking_task_pool,
            fee_history_cache,
            blocking_task_guard: BlockingTaskGuard::new(proof_permits),
            blocking_io_request_semaphore: Arc::new(Semaphore::new(max_blocking_io_requests)),
            raw_tx_sender,
            raw_tx_forwarder,
            converter,

            tx_batch_sender,
            pending_block_kind,
            send_raw_transaction_sync_timeout,
            evm_memory_limit,
        }
    }
}

impl BaseEthApiInner {
    /// Returns a handle to data on disk.
    #[inline]
    pub fn provider(&self) -> &BlockchainProvider {
        &self.components.provider
    }

    /// Returns a handle to the transaction response builder.
    #[inline]
    pub const fn converter(&self) -> &BaseRpcConverter {
        &self.converter
    }

    /// Returns a handle to data in memory.
    #[inline]
    pub const fn cache(&self) -> &EthStateCache {
        &self.eth_cache
    }

    /// Returns a handle to the pending block.
    #[inline]
    pub const fn pending_block(&self) -> &Mutex<Option<PendingBlock>> {
        &self.pending_block
    }

    /// Returns a handle to the task spawner.
    #[inline]
    pub const fn task_spawner(&self) -> &Runtime {
        &self.task_spawner
    }

    /// Returns a handle to the blocking thread pool.
    ///
    /// This is intended for tasks that are CPU bound.
    #[inline]
    pub const fn blocking_task_pool(&self) -> &BlockingTaskPool {
        &self.blocking_task_pool
    }

    /// Returns a handle to the EVM config.
    #[inline]
    pub fn evm_config(&self) -> &BaseEvmConfig {
        &self.components.evm_config
    }

    /// Returns a handle to the transaction pool.
    #[inline]
    pub fn pool(&self) -> &BaseTransactionPool {
        &self.components.pool
    }

    /// Returns the gas cap.
    #[inline]
    pub const fn gas_cap(&self) -> u64 {
        self.gas_cap
    }

    /// Returns the `max_simulate_blocks`.
    #[inline]
    pub const fn max_simulate_blocks(&self) -> u64 {
        self.max_simulate_blocks
    }

    /// Returns whether state roots are computed for `eth_simulateV1`.
    #[inline]
    pub const fn compute_state_root_for_eth_simulate(&self) -> bool {
        self.compute_state_root_for_eth_simulate
    }

    /// Returns a handle to the gas oracle.
    #[inline]
    pub const fn gas_oracle(&self) -> &GasPriceOracle<BlockchainProvider> {
        &self.gas_oracle
    }

    /// Returns a handle to the fee history cache.
    #[inline]
    pub const fn fee_history_cache(&self) -> &FeeHistoryCache {
        &self.fee_history_cache
    }

    /// Returns a handle to the signers.
    #[inline]
    pub const fn signers(&self) -> &SignersForRpc {
        &self.signers
    }

    /// Returns the starting block.
    #[inline]
    pub const fn starting_block(&self) -> U256 {
        self.starting_block
    }

    /// Returns the inner `Network`
    #[inline]
    pub fn network(&self) -> &base_execution_network_service::NetworkHandle {
        &self.components.network
    }

    /// The maximum number of blocks into the past for generating state proofs.
    #[inline]
    pub const fn eth_proof_window(&self) -> u64 {
        self.eth_proof_window
    }

    /// Returns reference to [`BlockingTaskGuard`].
    #[inline]
    pub const fn blocking_task_guard(&self) -> &BlockingTaskGuard {
        &self.blocking_task_guard
    }

    /// Returns [`broadcast::Receiver`] of new raw transactions
    #[inline]
    pub fn subscribe_to_raw_transactions(&self) -> broadcast::Receiver<Bytes> {
        self.raw_tx_sender.subscribe()
    }

    /// Broadcasts raw transaction if there are active subscribers.
    #[inline]
    pub fn broadcast_raw_transaction(&self, raw_tx: Bytes) {
        let _ = self.raw_tx_sender.send(raw_tx);
    }

    /// Returns the transaction batch sender
    #[inline]
    pub const fn tx_batch_sender(&self) -> &mpsc::UnboundedSender<BatchTxRequest> {
        &self.tx_batch_sender
    }

    /// Adds an _unvalidated_ transaction into the pool via the transaction batch sender.
    #[inline]
    pub async fn add_pool_transaction(
        &self,
        origin: base_execution_txpool::TransactionOrigin,
        transaction: base_execution_txpool::BasePooledTransaction,
    ) -> Result<AddedTransactionOutcome, EthApiError> {
        let (response_tx, response_rx) = tokio::sync::oneshot::channel();
        let request = base_execution_txpool::BatchTxRequest::new(origin, transaction, response_tx);

        self.tx_batch_sender().send(request).map_err(|_| crate::EthApiError::BatchTxSendError)?;

        Ok(response_rx.await??)
    }

    /// Returns the pending block kind
    #[inline]
    pub const fn pending_block_kind(&self) -> PendingBlockKind {
        self.pending_block_kind
    }

    /// Returns a handle to the raw transaction forwarder.
    #[inline]
    pub const fn raw_tx_forwarder(&self) -> Option<&RpcClient> {
        self.raw_tx_forwarder.as_ref()
    }

    /// Returns the timeout duration for `send_raw_transaction_sync` RPC method.
    #[inline]
    pub const fn send_raw_transaction_sync_timeout(&self) -> Duration {
        self.send_raw_transaction_sync_timeout
    }

    /// Returns the EVM memory limit.
    #[inline]
    pub const fn evm_memory_limit(&self) -> u64 {
        self.evm_memory_limit
    }

    /// Returns a reference to the blocking IO request semaphore.
    #[inline]
    pub const fn blocking_io_request_semaphore(&self) -> &Arc<Semaphore> {
        &self.blocking_io_request_semaphore
    }
}

#[cfg(test)]
mod tests {
    use alloy_eips::BlockNumberOrTag;
    use alloy_primitives::{B256, Signature, U64};
    use base_common_chain_config::ChainSpecProvider;
    use base_common_types_chain::{BaseTxEnvelope as TransactionSigned, Block, BlockBody, Header};
    use base_common_types_rpc::{Bundle, FeeHistory, StateContext, TransactionRequest};
    use base_execution_state_provider::test_utils::MockEthProvider;
    use base_testing_support::generators;
    use jsonrpsee_types::error::INVALID_PARAMS_CODE;
    use rand::Rng;

    use crate::{BaseEthApi, EthApiServer};

    type FakeEthApi = BaseEthApi;

    fn build_test_eth_api(provider: MockEthProvider) -> FakeEthApi {
        crate::test_utils::RpcTestUtils::api_builder(provider.clone()).build()
    }

    // Function to prepare the BaseEthApi with mock data
    fn prepare_eth_api(
        newest_block: u64,
        mut oldest_block: Option<B256>,
        block_count: u64,
        mock_provider: MockEthProvider,
    ) -> (FakeEthApi, Vec<u128>, Vec<f64>) {
        let mut rng = generators::rng();

        // Build mock data
        let mut gas_used_ratios = Vec::with_capacity(block_count as usize);
        let mut base_fees_per_gas = Vec::with_capacity(block_count as usize);
        let mut last_header = None;
        let mut parent_hash = B256::default();

        for i in (0..block_count).rev() {
            let hash = rng.random();
            // Note: Generates saner values to avoid invalid overflows later
            let gas_limit = rng.random::<u32>() as u64;
            let base_fee_per_gas: Option<u64> =
                rng.random::<bool>().then(|| rng.random::<u32>() as u64);
            let gas_used = rng.random::<u32>() as u64;

            let header = Header {
                number: newest_block - i,
                gas_limit,
                gas_used,
                base_fee_per_gas,
                parent_hash,
                ..Default::default()
            };
            last_header = Some(header.clone());
            parent_hash = hash;

            const TOTAL_TRANSACTIONS: usize = 100;
            let mut transactions = Vec::with_capacity(TOTAL_TRANSACTIONS);
            for _ in 0..TOTAL_TRANSACTIONS {
                let random_fee: u128 = rng.random();

                if let Some(base_fee_per_gas) = header.base_fee_per_gas {
                    let transaction = TransactionSigned::new_unhashed(
                        base_common_types_chain::BaseTypedTransaction::Eip1559(
                            base_common_types_chain::TxEip1559 {
                                max_priority_fee_per_gas: random_fee,
                                max_fee_per_gas: random_fee + base_fee_per_gas as u128,
                                ..Default::default()
                            },
                        ),
                        Signature::test_signature(),
                    );

                    transactions.push(transaction);
                } else {
                    let transaction = TransactionSigned::new_unhashed(
                        base_common_types_chain::BaseTypedTransaction::Legacy(Default::default()),
                        Signature::test_signature(),
                    );

                    transactions.push(transaction);
                }
            }

            mock_provider.add_block(
                hash,
                Block {
                    header: header.clone(),
                    body: BlockBody { transactions, ..Default::default() },
                },
            );
            mock_provider.add_header(hash, header);

            oldest_block.get_or_insert(hash);
            gas_used_ratios.push(gas_used as f64 / gas_limit as f64);
            base_fees_per_gas.push(base_fee_per_gas.map(|fee| fee as u128).unwrap_or_default());
        }

        // Add final base fee (for the next block outside of the request)
        let last_header = last_header.unwrap();
        let spec = mock_provider.chain_spec();
        base_fees_per_gas.push(
            spec.next_block_base_fee(&last_header, last_header.timestamp).unwrap_or_default()
                as u128,
        );

        let eth_api = build_test_eth_api(mock_provider);

        (eth_api, base_fees_per_gas, gas_used_ratios)
    }

    /// Invalid block range
    #[tokio::test]
    async fn test_fee_history_genesis() {
        let response = <BaseEthApi as EthApiServer>::fee_history(
            &build_test_eth_api(MockEthProvider::default()),
            U64::from(1),
            BlockNumberOrTag::Latest,
            None,
        )
        .await;
        let response = response.expect("genesis fee history");
        assert_eq!(response.oldest_block, 0);
        assert_eq!(response.base_fee_per_gas.len(), 2);
        assert_eq!(response.gas_used_ratio, vec![0.0]);
    }

    #[tokio::test]
    /// Invalid block range (request is before genesis)
    async fn test_fee_history_invalid_block_range_before_genesis() {
        let block_count = 10;
        let newest_block = 1337;
        let oldest_block = None;

        let (eth_api, _, _) =
            prepare_eth_api(newest_block, oldest_block, block_count, MockEthProvider::default());

        let response = <BaseEthApi as EthApiServer>::fee_history(
            &eth_api,
            U64::from(newest_block + 1),
            newest_block.into(),
            Some(vec![10.0]),
        )
        .await;

        assert!(response.is_err());
        let error_object = response.unwrap_err();
        assert_eq!(error_object.code(), INVALID_PARAMS_CODE);
    }

    #[tokio::test]
    /// Invalid block range (request is in the future)
    async fn test_fee_history_invalid_block_range_in_future() {
        let block_count = 10;
        let newest_block = 1337;
        let oldest_block = None;

        let (eth_api, _, _) =
            prepare_eth_api(newest_block, oldest_block, block_count, MockEthProvider::default());

        let response = <BaseEthApi as EthApiServer>::fee_history(
            &eth_api,
            U64::from(1),
            (newest_block + 1000).into(),
            Some(vec![10.0]),
        )
        .await;

        assert!(response.is_err());
        let error_object = response.unwrap_err();
        assert_eq!(error_object.code(), INVALID_PARAMS_CODE);
    }

    #[tokio::test]
    async fn test_call_many_reports_missing_block_number() {
        let eth_api = build_test_eth_api(MockEthProvider::default());
        let bundles = vec![Bundle {
            transactions: vec![TransactionRequest::default().into()],
            block_override: None,
        }];

        let response = <BaseEthApi as EthApiServer>::call_many(
            &eth_api,
            bundles,
            Some(StateContext {
                block_number: Some(BlockNumberOrTag::Number(100).into()),
                ..Default::default()
            }),
            None,
        )
        .await;

        let err =
            response.expect_err("call_many should fail when requested block number is missing");
        let message = err.message().to_ascii_lowercase();
        assert!(
            message.contains("block not found"),
            "missing block number should map to block-not-found: {message}"
        );
        assert!(
            !message.contains("best block does not exist"),
            "provider implementation detail should not leak from converted error: {message}"
        );
    }

    #[tokio::test]
    async fn test_call_many_keeps_header_not_found_when_block_hash_absent() {
        let eth_api = build_test_eth_api(MockEthProvider::default());
        let bundles = vec![Bundle {
            transactions: vec![TransactionRequest::default().into()],
            block_override: None,
        }];

        let response = <BaseEthApi as EthApiServer>::call_many(
            &eth_api,
            bundles,
            Some(StateContext {
                block_number: Some(B256::repeat_byte(42).into()),
                ..Default::default()
            }),
            None,
        )
        .await;

        let err =
            response.expect_err("call_many should fail when requested block hash is unavailable");
        let message = err.message().to_ascii_lowercase();
        assert!(
            message.contains("block not found"),
            "missing block hash should still map to block-not-found: {message}"
        );
    }

    #[tokio::test]
    /// Requesting no block should result in a default response
    async fn test_fee_history_no_block_requested() {
        let block_count = 10;
        let newest_block = 1337;
        let oldest_block = None;

        let (eth_api, _, _) =
            prepare_eth_api(newest_block, oldest_block, block_count, MockEthProvider::default());

        let response = <BaseEthApi as EthApiServer>::fee_history(
            &eth_api,
            U64::from(0),
            newest_block.into(),
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            response,
            FeeHistory::default(),
            "none: requesting no block should yield a default response"
        );
    }

    #[tokio::test]
    /// Requesting a single block should return 1 block (+ base fee for the next block over)
    async fn test_fee_history_single_block() {
        let block_count = 10;
        let newest_block = 1337;
        let oldest_block = None;

        let (eth_api, base_fees_per_gas, gas_used_ratios) =
            prepare_eth_api(newest_block, oldest_block, block_count, MockEthProvider::default());

        let fee_history =
            EthApiServer::fee_history(&eth_api, U64::from(1), newest_block.into(), None)
                .await
                .unwrap();
        assert_eq!(
            fee_history.base_fee_per_gas,
            &base_fees_per_gas[base_fees_per_gas.len() - 2..],
            "one: base fee per gas is incorrect"
        );
        assert_eq!(
            fee_history.base_fee_per_gas.len(),
            2,
            "one: should return base fee of the next block as well"
        );
        assert_eq!(
            &fee_history.gas_used_ratio,
            &gas_used_ratios[gas_used_ratios.len() - 1..],
            "one: gas used ratio is incorrect"
        );
        assert_eq!(fee_history.oldest_block, newest_block, "one: oldest block is incorrect");
        assert!(
            fee_history.reward.is_none(),
            "one: no percentiles were requested, so there should be no rewards result"
        );
    }

    /// Requesting all blocks should be ok
    #[tokio::test]
    async fn test_fee_history_all_blocks() {
        let block_count = 10;
        let newest_block = 1337;
        let oldest_block = None;

        let (eth_api, base_fees_per_gas, gas_used_ratios) =
            prepare_eth_api(newest_block, oldest_block, block_count, MockEthProvider::default());

        let fee_history =
            EthApiServer::fee_history(&eth_api, U64::from(block_count), newest_block.into(), None)
                .await
                .unwrap();

        assert_eq!(
            &fee_history.base_fee_per_gas, &base_fees_per_gas,
            "all: base fee per gas is incorrect"
        );
        assert_eq!(
            fee_history.base_fee_per_gas.len() as u64,
            block_count + 1,
            "all: should return base fee of the next block as well"
        );
        assert_eq!(
            &fee_history.gas_used_ratio, &gas_used_ratios,
            "all: gas used ratio is incorrect"
        );
        assert_eq!(
            fee_history.oldest_block,
            newest_block - block_count + 1,
            "all: oldest block is incorrect"
        );
        assert!(
            fee_history.reward.is_none(),
            "all: no percentiles were requested, so there should be no rewards result"
        );
    }
}
