//! Builder-local `base_getValidityTransactionStatus` RPC backed by durable block metadata.
//!
//! A lookup first resolves the transaction through the node's canonical chain provider and
//! confirms the containing block hash is still canonical at its height; only then does it read
//! the metadata record for that exact block hash. Orphaned, pending or unknown transactions
//! therefore return `null` even when metadata for their block exists. Results are a snapshot at
//! lookup time, not a finality guarantee.
//!
//! Each provider read observes its own canonical snapshot. A reorg between the reads can make a
//! lookup fail closed with an internal error (or `null`) instead of a classification; clients may
//! retry such errors once the chain settles.

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use alloy_consensus::transaction::TxHashRef;
use alloy_primitives::{B256, TxHash};
use base_execution_validity_metadata::{ValidityMetadataMetrics, ValidityMetadataStore};
use jsonrpsee::{
    core::{RpcResult, async_trait},
    proc_macros::rpc,
    types::{ErrorCode, ErrorObjectOwned},
};
use reth_storage_api::{BlockBodyIndicesProvider, TransactionsProvider};
use reth_tasks::Runtime;
use serde::{Deserialize, Serialize};
use tokio::sync::{Semaphore, oneshot};
use tracing::warn;

/// JSON-RPC error code returned when a canonical transaction's block has no usable metadata.
pub const VALIDITY_METADATA_UNAVAILABLE_CODE: i32 = -32050;

/// JSON-RPC error message paired with [`VALIDITY_METADATA_UNAVAILABLE_CODE`].
pub const VALIDITY_METADATA_UNAVAILABLE_MESSAGE: &str = "validity metadata unavailable";

/// JSON-RPC error data paired with [`VALIDITY_METADATA_UNAVAILABLE_CODE`].
pub const VALIDITY_METADATA_NOT_RECORDED: &str = "not_recorded";

/// Maximum number of status lookups performing blocking reads at the same time.
pub const MAX_CONCURRENT_VALIDITY_STATUS_LOOKUPS: usize = 16;

/// Validity classification of a canonically included transaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ValidityTransactionStatus {
    /// Queried transaction hash.
    pub transaction_hash: TxHash,
    /// Hash of the canonical block containing the transaction.
    pub block_hash: B256,
    /// Number of the canonical block containing the transaction.
    #[serde(with = "alloy_serde::quantity")]
    pub block_number: u64,
    /// Index of the transaction within its block.
    #[serde(with = "alloy_serde::quantity")]
    pub transaction_index: u64,
    /// Whether the builder committed the transaction with nonempty validity predicates.
    pub is_validity_transaction: bool,
}

/// Read-only RPC API for builder-recorded validity transaction classification.
#[rpc(server, namespace = "base")]
pub trait ValidityTransactionStatusApi {
    /// Returns the validity classification of a canonically included transaction.
    ///
    /// Returns `null` when the transaction is not canonically included in this node's chain,
    /// error [`VALIDITY_METADATA_UNAVAILABLE_CODE`] when it is included but its block has no
    /// recorded metadata, and an internal error when metadata is corrupt, conflicting or
    /// inconsistent with the canonical block.
    #[method(name = "getValidityTransactionStatus")]
    async fn get_validity_transaction_status(
        &self,
        tx_hash: TxHash,
    ) -> RpcResult<Option<ValidityTransactionStatus>>;
}

/// Implementation of [`ValidityTransactionStatusApiServer`].
///
/// Provider and store reads run on the blocking task pool, bounded by
/// [`MAX_CONCURRENT_VALIDITY_STATUS_LOOKUPS`]. Internal failures are counted in
/// `builder.validity_metadata.status_lookup_failures` and logged only on the first and every
/// power-of-two failure, so callers cannot amplify logs by repeating failing requests.
#[derive(Debug, Clone)]
pub struct ValidityTransactionStatusApiImpl<Provider> {
    provider: Provider,
    store: Option<ValidityMetadataStore>,
    task_spawner: Runtime,
    lookups: Arc<Semaphore>,
    internal_failures: Arc<AtomicU64>,
}

impl<Provider> ValidityTransactionStatusApiImpl<Provider>
where
    Provider: TransactionsProvider + BlockBodyIndicesProvider + Clone + Sync + 'static,
{
    /// Creates the API. A `None` store reports every canonical transaction as not recorded.
    pub fn new(
        provider: Provider,
        store: Option<ValidityMetadataStore>,
        task_spawner: Runtime,
    ) -> Self {
        Self {
            provider,
            store,
            task_spawner,
            lookups: Arc::new(Semaphore::new(MAX_CONCURRENT_VALIDITY_STATUS_LOOKUPS)),
            internal_failures: Arc::default(),
        }
    }

    /// Performs the blocking canonical lookup and metadata read for `tx_hash`.
    pub fn lookup(&self, tx_hash: TxHash) -> RpcResult<Option<ValidityTransactionStatus>> {
        let provider = &self.provider;
        let Some((transaction, meta)) = provider
            .transaction_by_hash_with_meta(tx_hash)
            .map_err(|error| self.internal(tx_hash, "provider", &error))?
        else {
            return Ok(None);
        };
        if *transaction.tx_hash() != tx_hash {
            return Err(self.internal(tx_hash, "transaction_mismatch", transaction.tx_hash()));
        }
        let body_indices = provider
            .block_body_indices(meta.block_number)
            .map_err(|error| self.internal(tx_hash, "provider", &error))?;
        // Check canonicality after reading the body so a reorg between the reads cannot pair the
        // count of one block with the hash of another without failing the consistency check.
        let canonical_hash = provider
            .block_hash(meta.block_number)
            .map_err(|error| self.internal(tx_hash, "provider", &error))?;
        if canonical_hash != Some(meta.block_hash) {
            return Ok(None);
        }

        let Some(store) = &self.store else {
            return Err(Self::not_recorded());
        };
        let Some(record) = store
            .get(meta.block_hash)
            .map_err(|error| self.internal(tx_hash, "storage", &error))?
        else {
            return Err(Self::not_recorded());
        };
        let Some(body_indices) = body_indices else {
            return Err(self.internal(tx_hash, "missing_body", &meta.block_number));
        };
        if record.block_number != meta.block_number
            || u64::from(record.transaction_count) != body_indices.tx_count
            || meta.index >= body_indices.tx_count
        {
            return Err(self.internal(tx_hash, "inconsistent", &meta.block_hash));
        }

        Ok(Some(ValidityTransactionStatus {
            transaction_hash: tx_hash,
            block_hash: meta.block_hash,
            block_number: meta.block_number,
            transaction_index: meta.index,
            is_validity_transaction: record.contains(&tx_hash),
        }))
    }

    /// Returns the retryable "metadata unavailable" error.
    pub fn not_recorded() -> ErrorObjectOwned {
        ErrorObjectOwned::owned(
            VALIDITY_METADATA_UNAVAILABLE_CODE,
            VALIDITY_METADATA_UNAVAILABLE_MESSAGE,
            Some(VALIDITY_METADATA_NOT_RECORDED),
        )
    }

    /// Counts an internal failure, logs a sample of failures locally, and returns a generic
    /// internal error carrying no diagnostics.
    ///
    /// `reason` must be one of the `status_lookup_failures` metric labels.
    fn internal(
        &self,
        tx_hash: TxHash,
        reason: &'static str,
        detail: &dyn std::fmt::Display,
    ) -> ErrorObjectOwned {
        ValidityMetadataMetrics::status_lookup_failures(reason).increment(1);
        let previous = self.internal_failures.fetch_add(1, Ordering::Relaxed);
        let failures = previous.saturating_add(1);
        if ValidityMetadataMetrics::is_log_sampled(previous, failures) {
            warn!(
                tx_hash = %tx_hash,
                reason,
                detail = %detail,
                failures,
                "validity status lookup failed"
            );
        }
        ErrorObjectOwned::from(ErrorCode::InternalError)
    }
}

#[async_trait]
impl<Provider> ValidityTransactionStatusApiServer for ValidityTransactionStatusApiImpl<Provider>
where
    Provider: TransactionsProvider + BlockBodyIndicesProvider + Clone + Sync + 'static,
{
    async fn get_validity_transaction_status(
        &self,
        tx_hash: TxHash,
    ) -> RpcResult<Option<ValidityTransactionStatus>> {
        // The permit moves into the blocking task so abandoned requests keep counting against the
        // limit until their read finishes.
        let permit = Arc::clone(&self.lookups)
            .acquire_owned()
            .await
            .map_err(|error| self.internal(tx_hash, "task", &error))?;
        let this = self.clone();
        let (tx, rx) = oneshot::channel();
        self.task_spawner.spawn_blocking_task(async move {
            let _permit = permit;
            let _ = tx.send(this.lookup(tx_hash));
        });
        rx.await.map_err(|error| self.internal(tx_hash, "task", &error))?
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use alloy_consensus::{BlockBody, Header, TxEip1559};
    use alloy_primitives::{Address, Bytes, Signature, TxKind, U256};
    use base_execution_validity_metadata::ValidityBlockRecord;
    use metrics_util::debugging::{DebugValue, DebuggingRecorder};
    use reth_chain_state::{ExecutedBlock, NewCanonicalChain};
    use reth_ethereum_primitives::{Block, Transaction, TransactionSigned};
    use reth_primitives_traits::RecoveredBlock;
    use reth_provider::{
        BlockWriter, DatabaseProviderFactory,
        providers::BlockchainProvider,
        test_utils::{MockEthProvider, MockNodeTypesWithDB, create_test_provider_factory},
    };
    use reth_storage_api::DBProvider;
    use serde_json::{Value, json};
    use tracing_subscriber::{
        Layer,
        layer::{Context, SubscriberExt},
    };

    use super::*;

    const SENDER: Address = Address::repeat_byte(0x11);

    fn transaction(nonce: u64) -> TransactionSigned {
        let tx = TxEip1559 {
            chain_id: 1,
            nonce,
            gas_limit: 21_000,
            to: TxKind::Call(Address::repeat_byte(0x22)),
            value: U256::from(1),
            ..Default::default()
        };
        TransactionSigned::new_unhashed(Transaction::Eip1559(tx), Signature::test_signature())
    }

    /// Builds a block; `salt` distinguishes sibling blocks at the same height.
    fn block(
        number: u64,
        parent_hash: B256,
        salt: u8,
        transactions: Vec<TransactionSigned>,
    ) -> RecoveredBlock<Block> {
        let senders = vec![SENDER; transactions.len()];
        let header = Header {
            number,
            parent_hash,
            extra_data: Bytes::from(vec![salt]),
            ..Default::default()
        };
        let body = BlockBody { transactions, ommers: Vec::new(), withdrawals: None };
        RecoveredBlock::new_unhashed(Block { header, body }, senders)
    }

    fn executed(block: &RecoveredBlock<Block>) -> ExecutedBlock {
        ExecutedBlock { recovered_block: Arc::new(block.clone()), ..Default::default() }
    }

    fn tx_hash(block: &RecoveredBlock<Block>, index: usize) -> TxHash {
        *block.body().transactions[index].tx_hash()
    }

    fn record(block: &RecoveredBlock<Block>, validity: &[usize]) -> ValidityBlockRecord {
        ValidityBlockRecord::new(
            block.hash(),
            block.header().number,
            block.body().transactions.len() as u32,
            validity.iter().map(|index| tx_hash(block, *index)).collect(),
        )
    }

    /// A real provider whose block 1 is persisted to the database and whose block 2 is
    /// canonical in memory, plus an unpublished sibling of block 2.
    struct Chain {
        provider: BlockchainProvider<MockNodeTypesWithDB>,
        persisted: RecoveredBlock<Block>,
        head: RecoveredBlock<Block>,
        sibling: RecoveredBlock<Block>,
    }

    impl Chain {
        fn new() -> Self {
            let factory = create_test_provider_factory();
            let genesis = block(0, B256::ZERO, 0, Vec::new());
            let persisted = block(1, genesis.hash(), 0, vec![transaction(0), transaction(1)]);
            let provider_rw = factory.database_provider_rw().unwrap();
            provider_rw.insert_block(&genesis).unwrap();
            provider_rw.insert_block(&persisted).unwrap();
            provider_rw.commit().unwrap();

            let provider = BlockchainProvider::new(factory).unwrap();
            let head = block(2, persisted.hash(), 0, vec![transaction(2), transaction(3)]);
            let sibling = block(2, persisted.hash(), 1, vec![transaction(4)]);
            provider
                .canonical_in_memory_state()
                .update_chain(NewCanonicalChain::Commit { new: vec![executed(&head)] });
            Self { provider, persisted, head, sibling }
        }

        fn reorg_to_sibling(&self) {
            self.provider.canonical_in_memory_state().update_chain(NewCanonicalChain::Reorg {
                new: vec![executed(&self.sibling)],
                old: vec![executed(&self.head)],
            });
        }

        fn rpc(&self, store: Option<ValidityMetadataStore>) -> jsonrpsee::RpcModule<()> {
            ValidityTransactionStatusApiImpl::new(self.provider.clone(), store, Runtime::test())
                .into_rpc()
                .remove_context()
        }
    }

    fn open_store() -> (tempfile::TempDir, ValidityMetadataStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = ValidityMetadataStore::open(dir.path().join("validity.redb")).unwrap();
        (dir, store)
    }

    async fn call(rpc: &jsonrpsee::RpcModule<()>, params: Value) -> Value {
        let request = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "base_getValidityTransactionStatus",
            "params": params,
        })
        .to_string();
        let (response, _) = rpc.raw_json_request(&request, 1).await.unwrap();
        serde_json::from_str(response.get()).unwrap()
    }

    fn assert_not_recorded(response: &Value) {
        assert_eq!(
            response["error"],
            json!({
                "code": -32050,
                "message": "validity metadata unavailable",
                "data": "not_recorded",
            })
        );
        assert!(response.get("result").is_none());
    }

    fn assert_internal_without_details(response: &Value) {
        assert_eq!(response["error"], json!({ "code": -32603, "message": "Internal error" }));
        assert!(response.get("result").is_none());
    }

    #[tokio::test]
    async fn reports_true_and_false_with_exact_quantities() {
        let chain = Chain::new();
        let (_dir, store) = open_store();
        store.put_batch(&[record(&chain.persisted, &[1]), record(&chain.head, &[])]).unwrap();
        let rpc = chain.rpc(Some(store));

        let validity = tx_hash(&chain.persisted, 1);
        let response = call(&rpc, json!([validity])).await;
        assert_eq!(
            response["result"],
            json!({
                "transactionHash": validity,
                "blockHash": chain.persisted.hash(),
                "blockNumber": "0x1",
                "transactionIndex": "0x1",
                "isValidityTransaction": true,
            })
        );

        let plain = tx_hash(&chain.persisted, 0);
        let response = call(&rpc, json!([plain])).await;
        assert_eq!(response["result"]["transactionIndex"], "0x0");
        assert_eq!(response["result"]["isValidityTransaction"], false);

        // In-memory canonical head, recorded with no validity transactions.
        let head_tx = tx_hash(&chain.head, 1);
        let response = call(&rpc, json!([head_tx])).await;
        assert_eq!(
            response["result"],
            json!({
                "transactionHash": head_tx,
                "blockHash": chain.head.hash(),
                "blockNumber": "0x2",
                "transactionIndex": "0x1",
                "isValidityTransaction": false,
            })
        );
    }

    #[tokio::test]
    async fn unknown_transaction_is_null_even_with_metadata() {
        let chain = Chain::new();
        let (_dir, store) = open_store();
        store.put_batch(&[record(&chain.persisted, &[0, 1])]).unwrap();
        let response = call(&chain.rpc(Some(store)), json!([B256::repeat_byte(0x99)])).await;
        assert_eq!(response["result"], Value::Null);
        assert!(response.get("error").is_none());
    }

    #[tokio::test]
    async fn missing_or_disabled_metadata_is_unavailable_not_false() {
        let chain = Chain::new();
        let (_dir, store) = open_store();
        store.put_batch(&[record(&chain.persisted, &[])]).unwrap();
        let rpc = chain.rpc(Some(store));
        assert_not_recorded(&call(&rpc, json!([tx_hash(&chain.head, 0)])).await);

        let disabled = chain.rpc(None);
        assert_not_recorded(&call(&disabled, json!([tx_hash(&chain.persisted, 0)])).await);
        let response = call(&disabled, json!([B256::repeat_byte(0x99)])).await;
        assert_eq!(response["result"], Value::Null);
    }

    #[tokio::test]
    async fn orphan_records_never_reveal_status_and_reorgs_select_matching_record() {
        let chain = Chain::new();
        let (_dir, store) = open_store();
        store.put_batch(&[record(&chain.head, &[0]), record(&chain.sibling, &[0])]).unwrap();
        let rpc = chain.rpc(Some(store));

        let head_tx = tx_hash(&chain.head, 0);
        let sibling_tx = tx_hash(&chain.sibling, 0);
        assert_eq!(call(&rpc, json!([head_tx])).await["result"]["isValidityTransaction"], true);
        let orphan = call(&rpc, json!([sibling_tx])).await;
        assert_eq!(orphan["result"], Value::Null);
        assert!(orphan.get("error").is_none());

        chain.reorg_to_sibling();
        assert_eq!(call(&rpc, json!([head_tx])).await["result"], Value::Null);
        let response = call(&rpc, json!([sibling_tx])).await;
        assert_eq!(response["result"]["blockHash"], json!(chain.sibling.hash()));
        assert_eq!(response["result"]["blockNumber"], "0x2");
        assert_eq!(response["result"]["isValidityTransaction"], true);
    }

    #[tokio::test]
    async fn metadata_persists_across_store_reopen() {
        let chain = Chain::new();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("validity.redb");
        ValidityMetadataStore::open(&path)
            .unwrap()
            .put_batch(&[record(&chain.persisted, &[0])])
            .unwrap();
        let rpc = chain.rpc(Some(ValidityMetadataStore::open(&path).unwrap()));
        let response = call(&rpc, json!([tx_hash(&chain.persisted, 0)])).await;
        assert_eq!(response["result"]["isValidityTransaction"], true);
    }

    #[tokio::test]
    async fn conflicting_metadata_is_internal_error_without_details() {
        let chain = Chain::new();
        let (_dir, store) = open_store();
        store.put_batch(&[record(&chain.persisted, &[0])]).unwrap();
        store.put_batch(&[record(&chain.persisted, &[1])]).unwrap();
        let response = call(&chain.rpc(Some(store)), json!([tx_hash(&chain.persisted, 1)])).await;
        assert_internal_without_details(&response);
    }

    #[tokio::test]
    async fn metadata_inconsistent_with_canonical_block_is_internal_error() {
        let chain = Chain::new();
        let mut wrong_count = record(&chain.persisted, &[]);
        wrong_count.transaction_count = 3;
        let mut wrong_number = record(&chain.head, &[]);
        wrong_number.block_number = 7;
        let (_dir, store) = open_store();
        store.put_batch(&[wrong_count, wrong_number]).unwrap();
        let rpc = chain.rpc(Some(store));

        assert_internal_without_details(&call(&rpc, json!([tx_hash(&chain.persisted, 0)])).await);
        assert_internal_without_details(&call(&rpc, json!([tx_hash(&chain.head, 0)])).await);
    }

    #[tokio::test]
    async fn malformed_hash_params_are_rejected() {
        let chain = Chain::new();
        let rpc = chain.rpc(None);
        for params in [
            json!([]),
            json!(["0x1234"]),
            json!([format!("0x{}", "zz".repeat(32))]),
            json!([format!("0x{}", "00".repeat(33))]),
            json!([42]),
        ] {
            let response = call(&rpc, params.clone()).await;
            assert_eq!(response["error"]["code"], -32602, "params {params} must be rejected");
            assert!(response.get("result").is_none());
        }
    }

    type MockApi = ValidityTransactionStatusApiImpl<MockEthProvider>;

    fn mock_api(provider: &MockEthProvider, store: Option<ValidityMetadataStore>) -> MockApi {
        ValidityTransactionStatusApiImpl::new(provider.clone(), store, Runtime::test())
    }

    /// A provider whose transaction index still returns a stale block replaced at its height.
    fn mock_with_stale_index(
        stale: &RecoveredBlock<Block>,
        canonical: &RecoveredBlock<Block>,
        tx_count: u64,
    ) -> MockEthProvider {
        let provider = MockEthProvider::default();
        provider.blocks.lock().insert(stale.hash(), stale.clone().into_block());
        provider.add_header(canonical.hash(), canonical.header().clone());
        provider.add_block_body_indices(stale.header().number, Default::default());
        provider.block_body_indices.lock().get_mut(&stale.header().number).unwrap().tx_count =
            tx_count;
        provider
    }

    /// Tracing layer counting `WARN` and `ERROR` events emitted on the current thread.
    #[derive(Clone, Default)]
    struct WarnCounter(Arc<AtomicU64>);

    impl<S: tracing::Subscriber> Layer<S> for WarnCounter {
        fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
            if *event.metadata().level() <= tracing::Level::WARN {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
    }

    /// Yields until `condition` holds; the timeout only guards against a hung test.
    async fn wait_until(mut condition: impl FnMut() -> bool) {
        tokio::time::timeout(Duration::from_secs(30), async {
            while !condition() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("condition not reached");
    }

    #[tokio::test]
    async fn explicit_canonical_hash_check_hides_stale_index_entries() {
        let stale = block(5, B256::ZERO, 0, vec![transaction(0)]);
        let canonical = block(5, B256::ZERO, 1, vec![transaction(1)]);
        let provider = mock_with_stale_index(&stale, &canonical, 1);
        let (_dir, store) = open_store();
        store.put_batch(&[record(&stale, &[0])]).unwrap();

        let result = mock_api(&provider, Some(store)).lookup(tx_hash(&stale, 0)).unwrap();
        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn index_outside_canonical_count_or_missing_body_is_internal_error() {
        let block = block(5, B256::ZERO, 0, vec![transaction(0), transaction(1)]);
        let (_dir, store) = open_store();
        let mut truncated = record(&block, &[]);
        truncated.transaction_count = 1;
        store.put_batch(&[truncated]).unwrap();

        let provider = mock_with_stale_index(&block, &block, 1);
        let api = mock_api(&provider, Some(store));
        let error = api.lookup(tx_hash(&block, 1)).unwrap_err();
        assert_eq!(error.code(), ErrorCode::InternalError.code());
        assert!(error.data().is_none());

        provider.block_body_indices.lock().clear();
        let error = api.lookup(tx_hash(&block, 0)).unwrap_err();
        assert_eq!(error.code(), ErrorCode::InternalError.code());
    }

    #[tokio::test]
    async fn reorg_between_provider_reads_fails_closed_instead_of_classifying() {
        // The transaction lookup saw block A (2 transactions, recorded with the transaction as a
        // validity transaction), but the body read observed a replacing block with 3
        // transactions before the chain returned to A. The lookup must not classify.
        let block = block(5, B256::ZERO, 0, vec![transaction(0), transaction(1)]);
        let (_dir, store) = open_store();
        store.put_batch(&[record(&block, &[0])]).unwrap();
        let provider = mock_with_stale_index(&block, &block, 3);

        let error = mock_api(&provider, Some(store)).lookup(tx_hash(&block, 0)).unwrap_err();
        assert_eq!(error.code(), ErrorCode::InternalError.code());
        assert!(error.data().is_none());
    }

    #[tokio::test]
    async fn repeated_internal_failures_are_counted_but_sampled_in_logs() {
        let metrics = DebuggingRecorder::new();
        let snapshotter = metrics.snapshotter();
        let _metrics_guard = metrics::set_default_local_recorder(&metrics);
        let warnings = WarnCounter::default();
        let _log_guard =
            tracing::subscriber::set_default(tracing_subscriber::registry().with(warnings.clone()));

        // A recorded canonical block whose body indices are missing fails as `missing_body`.
        let block = block(5, B256::ZERO, 0, vec![transaction(0)]);
        let provider = mock_with_stale_index(&block, &block, 1);
        provider.block_body_indices.lock().clear();
        let (_dir, store) = open_store();
        store.put_batch(&[record(&block, &[0])]).unwrap();
        let api = mock_api(&provider, Some(store));

        for _ in 0..100 {
            let error = api.lookup(tx_hash(&block, 0)).unwrap_err();
            assert_eq!(error, ErrorObjectOwned::from(ErrorCode::InternalError));
        }

        // Failures 1, 2, 4, 8, 16, 32 and 64 are logged; every failure is counted.
        assert_eq!(warnings.0.load(Ordering::SeqCst), 7);
        let failures =
            snapshotter.snapshot().into_vec().into_iter().find_map(|(key, _, _, value)| {
                let matches = key.key().name()
                    == "builder.validity_metadata.status_lookup_failures"
                    && key
                        .key()
                        .labels()
                        .any(|l| l.key() == "reason" && l.value() == "missing_body");
                match value {
                    DebugValue::Counter(count) if matches => Some(count),
                    _ => None,
                }
            });
        assert_eq!(failures, Some(100));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_lookups_are_bounded_and_abandoned_callers_keep_permits() {
        let provider = MockEthProvider::default();
        let api = mock_api(&provider, None);
        let call = |byte: u8| {
            let api = api.clone();
            tokio::spawn(async move {
                api.get_validity_transaction_status(B256::repeat_byte(byte)).await
            })
        };

        // Every lookup that reaches the provider blocks on its block-map lock, held by a separate
        // thread until released.
        let (release, released) = std::sync::mpsc::channel::<()>();
        let (locked, is_locked) = std::sync::mpsc::channel::<()>();
        let blocks = Arc::clone(&provider.blocks);
        let holder = std::thread::spawn(move || {
            let _held = blocks.lock();
            locked.send(()).unwrap();
            let _ = released.recv();
        });
        is_locked.recv().unwrap();
        let entered: Vec<_> = (0..MAX_CONCURRENT_VALIDITY_STATUS_LOOKUPS as u8).map(call).collect();
        wait_until(|| api.lookups.available_permits() == 0).await;

        // At the limit, an additional call cannot start a blocking lookup.
        let over_limit = call(0xff);
        for _ in 0..100 {
            tokio::task::yield_now().await;
        }
        assert!(!over_limit.is_finished());
        assert_eq!(api.lookups.available_permits(), 0);

        // Abandoning the in-flight callers does not free permits while their reads are blocked.
        for caller in &entered {
            caller.abort();
        }
        for caller in entered {
            assert!(caller.await.unwrap_err().is_cancelled());
        }
        assert_eq!(api.lookups.available_permits(), 0);
        assert!(!over_limit.is_finished());

        // Releasing the provider lets the abandoned reads finish and the waiting call proceed.
        release.send(()).unwrap();
        holder.join().unwrap();
        assert_eq!(over_limit.await.unwrap().unwrap(), None);
        let all = tokio::time::timeout(
            Duration::from_secs(30),
            api.lookups.acquire_many(MAX_CONCURRENT_VALIDITY_STATUS_LOOKUPS as u32),
        )
        .await
        .expect("permits returned")
        .unwrap();
        drop(all);
    }
}
