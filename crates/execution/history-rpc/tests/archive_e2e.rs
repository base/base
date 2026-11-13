//! End-to-end check of the shared chain history store on a short-retention node.
//!
//! One node serves the `archive_` export, prunes bodies, receipts and state
//! history older than [`RETENTION`] blocks, and answers `eth_` reads with the archive-backed
//! override. An in-process writer copies finalized blocks from the node's export into an
//! in-memory archive while the chain grows. Responses captured while the node still held the
//! history must equal, byte for byte as JSON, the responses the pruned node serves later.

use std::{collections::BTreeMap, sync::Arc, time::Duration};

use alloy_consensus::Sealed;
use alloy_eips::{Decodable2718, Encodable2718};
use alloy_genesis::{Genesis, GenesisAccount};
use alloy_primitives::{Address, B256, Bytes, U256};
use alloy_rpc_client::RpcClient;
use base_common_consensus::BaseTxEnvelope;
use base_common_precompiles::NonceManagerStorage;
use base_common_rpc_types::BaseTransactionRequest;
use base_execution_chainspec::BaseChainSpec;
use base_execution_eip8130_rpc_node::{Eip8130RpcExtension, TransactionCountOwner};
use base_history_rpc::{
    ArchiveApiImpl, ArchiveEthConfig, ArchiveEthExtension, ArchiveExportConfig,
    ArchiveExportExtension, ArchiveExportLimits, ArchiveSource, DEFAULT_MAX_CONCURRENT_RANGES,
    DEFAULT_STARTUP_CHECK_TIMEOUT, SenderSource,
};
use base_history_store::{
    ArchiveLayout, ArchiveWriter, CacheConfig, Catalog, CatalogStore, ChainIdentity, ChainSource,
    ChunkVerifier, DEFAULT_MAX_RANGE_BLOCKS, DEFAULT_MAX_RANGE_BYTES, DEFAULT_SNAPSHOT_TTL,
    RpcChainSource, SourcePool, StepOutcome, WriterConfig,
    test_utils::{Gate, GateOp, GatedStore, test_index_options},
};
use base_node_runner::test_utils::{
    DEFAULT_JWT_SECRET, L1_BLOCK_INFO_DEPOSIT_TX, LocalNodeRetention, TestHarness,
};
use base_protocol::BaseTimeUpdateTx;
use base_test_utils::{
    Account, build_test_genesis, build_test_genesis_cobalt, build_test_genesis_everest,
};
use object_store::{ObjectStore, RetryConfig, aws::AmazonS3Builder, memory::InMemory};
use reth_chainspec::EthChainSpec;
use reth_node_core::args::{PruningArgs, StaticFilesArgs};
use reth_prune_types::PruneSegment;
use reth_rpc_layer::JwtSecret;
use reth_storage_api::{
    AccountReader, BlockBodyIndicesProvider, BlockNumReader, BlockReader, PruneCheckpointReader,
    StateProviderFactory, TransactionsProvider, errors::ProviderError,
};
use serde_json::{Value, json};

/// JSON-RPC "method not found".
const METHOD_NOT_FOUND: i64 = -32601;
/// EIP-4444 "pruned history" error code.
const PRUNED_HISTORY: i64 = 4444;

/// Blocks per archive segment.
const BLOCKS_PER_SEGMENT: u64 = 8;
/// Blocks the node keeps; reth's minimum for body and receipt pruning.
const RETENTION: u64 = 64;
/// Blocks carrying transactions; everything after is empty.
const ACTIVE_BLOCKS: u64 = 6;
/// Final chain height; far enough past [`RETENTION`] that the active blocks are pruned.
const TIP: u64 = 96;
/// Upper bound for the whole end-to-end run.
const E2E_TIMEOUT: Duration = Duration::from_secs(240);

/// Runtime code: `sstore(0, calldataload(0)); log1(0, 0, calldataload(0))`.
const RUNTIME: [u8; 13] =
    [0x60, 0x00, 0x35, 0x80, 0x60, 0x00, 0x55, 0x60, 0x00, 0x60, 0x00, 0xa1, 0x00];

/// Init code returning [`RUNTIME`].
fn init_code() -> Bytes {
    let mut code = vec![0x60, 0x0d, 0x60, 0x0c, 0x60, 0x00, 0x39, 0x60, 0x0d, 0x60, 0x00, 0xf3];
    code.extend_from_slice(&RUNTIME);
    code.into()
}

/// The harness's L1 info deposit with a per-block source hash.
///
/// The shared constant has the same hash in every block, so the node's hash-keyed receipt lookup
/// returns the deposit nonce of the last block that included it; a unique hash keeps the
/// pre-pruning answers well defined.
fn l1_info_deposit(number: u64) -> Bytes {
    let BaseTxEnvelope::Deposit(deposit) =
        BaseTxEnvelope::decode_2718(&mut L1_BLOCK_INFO_DEPOSIT_TX.as_ref())
            .expect("deposit decodes")
    else {
        panic!("L1 info transaction must be a deposit");
    };
    let mut deposit = deposit.into_inner();
    deposit.source_hash = B256::from(U256::from(number));
    BaseTxEnvelope::Deposit(Sealed::new(deposit)).encoded_2718().into()
}

/// The `BaseTime` deposit for block `number`.
fn base_time_deposit(number: u64) -> Bytes {
    BaseTimeUpdateTx::new(200)
        .expect("200 ms component is valid")
        .into_deposit_tx(number)
        .encoded_2718()
        .into()
}

/// Settings of a node that keeps only the last [`RETENTION`] blocks of history. Static files
/// hold [`BLOCKS_PER_SEGMENT`] blocks each so pruning can drop whole files early.
fn short_retention() -> LocalNodeRetention {
    LocalNodeRetention {
        pruning: PruningArgs {
            block_interval: Some(1),
            minimum_distance: Some(RETENTION),
            bodies_distance: Some(RETENTION),
            receipts_distance: Some(RETENTION),
            account_history_distance: Some(RETENTION),
            storage_history_distance: Some(RETENTION),
            ..Default::default()
        },
        static_files: StaticFilesArgs {
            blocks_per_file_transactions: Some(BLOCKS_PER_SEGMENT),
            blocks_per_file_receipts: Some(BLOCKS_PER_SEGMENT),
            blocks_per_file_transaction_senders: Some(BLOCKS_PER_SEGMENT),
            blocks_per_file_account_change_sets: Some(BLOCKS_PER_SEGMENT),
            blocks_per_file_storage_change_sets: Some(BLOCKS_PER_SEGMENT),
            ..Default::default()
        },
    }
}

/// Settings of reth's `--full` sender handling (`--prune.sender-recovery.full`) with every other
/// exported table kept: the node drops each block's senders as soon as it is pruned.
fn senders_pruned() -> LocalNodeRetention {
    LocalNodeRetention {
        pruning: PruningArgs {
            block_interval: Some(1),
            sender_recovery_full: true,
            ..Default::default()
        },
        static_files: StaticFilesArgs {
            blocks_per_file_transaction_senders: Some(BLOCKS_PER_SEGMENT),
            ..Default::default()
        },
    }
}

/// Sends one JSON-RPC request and returns the result or the error rendered as JSON.
async fn call(client: &RpcClient, method: &'static str, params: Value) -> Value {
    match client.request::<_, Value>(method, params.clone()).await {
        Ok(value) => value,
        Err(err) => json!({ "error": err.to_string(), "method": method, "params": params }),
    }
}

/// Every request the test compares before and after pruning.
fn requests(
    blocks: &[(u64, B256)],
    tx_hashes: &[B256],
    contract: Address,
) -> Vec<(&'static str, Value)> {
    let mut out = Vec::new();
    for (number, hash) in blocks {
        let tag = format!("0x{number:x}");
        out.push(("eth_getBlockByNumber", json!([tag, true])));
        out.push(("eth_getBlockByNumber", json!([tag, false])));
        out.push(("eth_getBlockByHash", json!([hash, true])));
        for account in [Account::Alice.address(), Account::Bob.address(), contract] {
            out.push(("eth_getBalance", json!([account, tag])));
            out.push(("eth_getTransactionCount", json!([account, tag])));
            out.push(("eth_getCode", json!([account, tag])));
        }
        out.push(("eth_getTransactionCount", json!([Account::Alice.address(), tag, "0x0"])));
        out.push(("eth_getStorageAt", json!([contract, "0x0", tag])));
        out.push(("eth_getBalance", json!([Account::Alice.address(), { "blockHash": hash }])));
    }
    for hash in tx_hashes {
        out.push(("eth_getTransactionByHash", json!([hash])));
        out.push(("eth_getTransactionReceipt", json!([hash])));
    }
    let last = blocks.last().expect("blocks").0;
    out.push(("eth_getLogs", json!([{ "fromBlock": "0x1", "toBlock": format!("0x{last:x}") }])));
    out.push((
        "eth_getLogs",
        json!([{ "fromBlock": "0x1", "toBlock": format!("0x{last:x}"), "address": contract }]),
    ));
    out.push(("eth_getLogs", json!([{ "blockHash": blocks[2].1 }])));
    out
}

/// Runs writer steps until the next segment is not finalized yet.
async fn drain(writer: &ArchiveWriter) -> u64 {
    let mut published = 0;
    loop {
        match writer.step().await.expect("writer step") {
            StepOutcome::Published { .. } => published += 1,
            StepOutcome::NotReady { .. } => return published,
            StepOutcome::Discarded { segment } => {
                panic!("single writer discarded segment {segment}")
            }
        }
    }
}

/// A pruned node serves every covered `eth_` method from the archive with exact parity.
#[tokio::test(flavor = "multi_thread")]
async fn pruned_node_serves_history_from_archive() {
    tokio::time::timeout(E2E_TIMEOUT, pruned_node_parity(short_retention()))
        .await
        .expect("e2e run timed out");
}

/// Unequal retention: account history is kept but storage history is pruned. reth's
/// historical state provider then refuses account reads at old blocks as pruned although the
/// account checkpoint says they are local; those refusals go
/// to the archive and every response still matches the pre-pruning one.
#[tokio::test(flavor = "multi_thread")]
async fn unequal_retention_serves_refused_state_from_archive() {
    let mut retention = short_retention();
    retention.pruning.account_history_distance = None;
    tokio::time::timeout(E2E_TIMEOUT, pruned_node_parity(retention))
        .await
        .expect("e2e run timed out");
}

/// Body of the parity tests: builds a chain on a node with `retention`, archives it, and
/// compares every request before and after pruning.
async fn pruned_node_parity(retention: LocalNodeRetention) {
    let gated = GatedStore::new(Arc::new(InMemory::new()));
    let store: Arc<dyn ObjectStore> = Arc::new(gated.clone());
    let chain_spec = Arc::new(BaseChainSpec::from_genesis(build_test_genesis()));
    let harness = archive_node(Arc::clone(&store), Arc::clone(&chain_spec), retention).await;
    let client = harness.rpc_client().expect("rpc client");

    // Blocks 1..=ACTIVE_BLOCKS: a deployment, value transfers and contract calls that write
    // storage and emit logs.
    let (deploy, contract, deploy_hash) =
        Account::Deployer.create_deployment_tx(init_code(), 0).expect("deployment tx");
    let mut tx_hashes = vec![deploy_hash];
    for number in 1..=ACTIVE_BLOCKS {
        let mut txs = vec![l1_info_deposit(number), base_time_deposit(number)];
        if number == 1 {
            txs.push(deploy.clone());
        }
        let nonce = number - 1;
        let (transfer, transfer_hash) = Account::Alice
            .sign_txn_request(
                BaseTransactionRequest::default()
                    .to(Account::Bob.address())
                    .value(U256::from(1_000 * number))
                    .nonce(nonce * 2),
            )
            .expect("transfer tx");
        let (set, set_hash) = Account::Alice
            .sign_txn_request(
                BaseTransactionRequest::default()
                    .to(contract)
                    .input(B256::from(U256::from(100 + number)).0.to_vec().into())
                    .nonce(nonce * 2 + 1),
            )
            .expect("contract call tx");
        txs.extend([transfer, set]);
        tx_hashes.extend([transfer_hash, set_hash]);
        harness.build_block_from_transactions(txs).await.expect("active block");
    }
    harness.advance_chain(BLOCKS_PER_SEGMENT * 2 - ACTIVE_BLOCKS).await.expect("advance");

    let provider = harness.blockchain_provider();
    let blocks: Vec<(u64, B256)> = (1..=ACTIVE_BLOCKS + 2)
        .map(|n| {
            let block = provider.block_by_number(n).expect("read").expect("block exists");
            (n, alloy_primitives::Sealable::hash_slow(&block.header))
        })
        .collect();
    let requests = requests(&blocks, &tx_hashes, contract);
    let mut expected = Vec::with_capacity(requests.len());
    for (method, params) in &requests {
        expected.push(call(&client, method, params.clone()).await);
    }
    for ((method, params), value) in requests.iter().zip(&expected) {
        assert!(
            value.get("error").is_none() && !value.is_null(),
            "{method} {params} must succeed before pruning, got {value}"
        );
    }

    // Archive the chain while it grows past the retention window.
    let writer = archive_writer(Arc::clone(&store), &chain_spec, &harness).await;
    let mut published = drain(&writer).await;
    while harness.latest_block().number < TIP {
        harness.advance_chain(BLOCKS_PER_SEGMENT).await.expect("advance");
        published += drain(&writer).await;
    }
    assert_eq!(
        published,
        TIP / BLOCKS_PER_SEGMENT,
        "every segment ending at or below finalized block {} is published",
        TIP - 1
    );
    // Let the reader's index view catch up with the writer's last flush.
    tokio::time::sleep(Duration::from_millis(200)).await;

    // The node no longer has the active blocks' bodies or state.
    let pruned_state = provider
        .history_by_block_number(1)
        .and_then(|state| state.basic_account(&Account::Alice.address()));
    assert!(
        matches!(
            pruned_state,
            Err(ProviderError::StateAtBlockPruned(1)
                | ProviderError::InsufficientChangesets { requested: 1, .. })
        ),
        "state at block 1 must be pruned locally, got {pruned_state:?}"
    );
    let pruned_body = provider.block_by_number(1);
    assert!(
        !matches!(pruned_body, Ok(Some(_))),
        "body of block 1 must be pruned locally, got {pruned_body:?}"
    );

    for ((method, params), before) in requests.iter().zip(&expected) {
        let after = call(&client, method, params.clone()).await;
        assert_eq!(&after, before, "{method} {params} must match the pre-pruning response");
    }

    // `earliest` means the node's earliest block, not genesis, for archive-served state too.
    let earliest = provider.earliest_block_number().expect("earliest block");
    assert!(
        earliest > ACTIVE_BLOCKS,
        "the active blocks are below the node's earliest block {earliest}"
    );
    let alice = Account::Alice.address();
    let by_tag = call(&client, "eth_getBalance", json!([alice, "earliest"])).await;
    let by_number =
        call(&client, "eth_getBalance", json!([alice, format!("0x{earliest:x}")])).await;
    let genesis = call(&client, "eth_getBalance", json!([alice, "0x0"])).await;
    assert_eq!(by_tag, by_number, "earliest resolves to block {earliest}");
    assert_ne!(by_tag, genesis, "earliest is not genesis once bodies are pruned");

    // A block above the node's head is never looked up in the archive, even on a pruned node.
    // Only segment and catalog reads count: the index reader polls its manifest in the background.
    let mut gates = archive_lookup_gates(&gated);
    let head = harness.latest_block().number;
    let next =
        call(&client, "eth_getBlockByNumber", json!([format!("0x{:x}", head + 1), false])).await;
    assert!(next.is_null(), "block {} does not exist yet, got {next}", head + 1);
    assert!(!gates.any_reached(), "a block above the head must not reach the archive");

    // The export is not served on the public HTTP server.
    for (method, params) in
        [("archive_status", json!([])), ("archive_getBlockRange", json!([1, 1]))]
    {
        let err = client
            .request::<_, Value>(method, params)
            .await
            .expect_err("archive_ must not be on the public server");
        assert_eq!(
            err.as_error_resp().map(|payload| payload.code),
            Some(METHOD_NOT_FOUND),
            "{method} on public HTTP must be method-not-found, got {err}"
        );
    }
}

/// Released gates on the archive's segment objects and catalog, the paths a request lookup reads.
struct LookupGates(Vec<Gate>);

impl LookupGates {
    /// Whether any request reached a segment object or the catalog since the gates were armed.
    fn any_reached(&mut self) -> bool {
        self.0.iter_mut().any(|gate| gate.reached.take().expect("gate receiver").try_recv().is_ok())
    }
}

/// Arms and releases pass-through gates on `chain/seg/` and `chain/catalog.json`.
fn archive_lookup_gates(gated: &GatedStore) -> LookupGates {
    let prefixes = [
        format!("chain/{}/", ArchiveLayout::SEGMENTS),
        format!("chain/{}", ArchiveLayout::CATALOG),
    ];
    LookupGates(
        prefixes
            .iter()
            .map(|prefix| {
                let mut gate = gated.arm(GateOp::Any, prefix);
                gate.release();
                gate
            })
            .collect(),
    )
}

/// Starts a node with the `archive_` export (authenticated server only), `retention`, and the
/// archive-backed `eth_` reads over `store`.
async fn archive_node(
    store: Arc<dyn ObjectStore>,
    chain_spec: Arc<BaseChainSpec>,
    retention: LocalNodeRetention,
) -> TestHarness {
    TestHarness::builder()
        .with_chain_spec(chain_spec)
        .with_retention(retention)
        .with_ext::<ArchiveExportExtension>(ArchiveExportConfig {
            enabled: true,
            limits: ArchiveExportLimits {
                max_range_blocks: DEFAULT_MAX_RANGE_BLOCKS,
                max_range_bytes: DEFAULT_MAX_RANGE_BYTES,
                max_concurrent_ranges: DEFAULT_MAX_CONCURRENT_RANGES,
            },
        })
        .with_ext::<ArchiveEthExtension>(Some(ArchiveEthConfig {
            source: ArchiveSource { store, layout: ArchiveLayout::new("chain") },
            cache: CacheConfig::default(),
            index: test_index_options(),
            startup_check_timeout: DEFAULT_STARTUP_CHECK_TIMEOUT,
            snapshot_ttl: DEFAULT_SNAPSHOT_TTL,
        }))
        .with_ext::<Eip8130RpcExtension>(TransactionCountOwner::Archive)
        .build()
        .await
        .expect("node starts")
}

/// Opens a writer that reads the node's export over its JWT-authenticated RPC server.
async fn archive_writer(
    store: Arc<dyn ObjectStore>,
    chain_spec: &Arc<BaseChainSpec>,
    harness: &TestHarness,
) -> ArchiveWriter {
    let secret = JwtSecret::from_hex(DEFAULT_JWT_SECRET.to_string()).expect("test jwt secret");
    let source = RpcChainSource::with_jwt(harness.auth_url(), Duration::from_secs(10), secret)
        .expect("source");
    ArchiveWriter::open(
        store,
        ArchiveLayout::new("chain"),
        ChainIdentity {
            chain_id: chain_spec.chain().id(),
            genesis_hash: chain_spec.genesis_hash(),
            blocks_per_segment: BLOCKS_PER_SEGMENT,
        },
        SourcePool::new(vec![Arc::new(source)]),
        ChunkVerifier::new(Arc::clone(chain_spec)),
        WriterConfig { chunk_blocks: BLOCKS_PER_SEGMENT, idle_interval: Duration::from_millis(50) },
        test_index_options(),
    )
    .await
    .expect("writer opens")
}

/// Review7 P2: a node that prunes senders (reth's `--full`) is still a writer source. Its
/// status keeps the earliest local block at genesis, and its range export recovers each
/// sender from the transaction (deposits give their `from`), so the senders equal the ones the
/// node recorded at execution and the writer's `ChunkVerifier` accepts every segment.
#[tokio::test(flavor = "multi_thread")]
async fn node_pruning_senders_exports_recovered_senders() {
    tokio::time::timeout(E2E_TIMEOUT, async {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let chain_spec = Arc::new(BaseChainSpec::from_genesis(build_test_genesis()));
        let harness =
            archive_node(Arc::clone(&store), Arc::clone(&chain_spec), senders_pruned()).await;
        let sender_of = |raw: &Bytes| match BaseTxEnvelope::decode_2718(&mut raw.as_ref())
            .expect("tx decodes")
        {
            BaseTxEnvelope::Deposit(deposit) => deposit.from,
            _ => Account::Alice.address(),
        };
        let mut expected = Vec::new();
        for number in 1..=ACTIVE_BLOCKS {
            let (transfer, _) = Account::Alice
                .sign_txn_request(
                    BaseTransactionRequest::default()
                        .to(Account::Bob.address())
                        .value(U256::from(number))
                        .nonce(number - 1),
                )
                .expect("transfer tx");
            let txs = vec![l1_info_deposit(number), base_time_deposit(number), transfer];
            expected.push(txs.iter().map(sender_of).collect::<Vec<_>>());
            harness.build_block_from_transactions(txs).await.expect("active block");
        }
        harness.advance_chain(BLOCKS_PER_SEGMENT * 2 - ACTIVE_BLOCKS).await.expect("advance");

        // In `Full` mode reth never writes senders, and the checkpoint keeps naming the block
        // of its first pass.
        let provider = harness.blockchain_provider();
        let checkpoint = provider
            .get_prune_checkpoint(PruneSegment::SenderRecovery)
            .expect("checkpoint read")
            .expect("the pruner ran");
        assert!(checkpoint.prune_mode.is_full(), "senders are pruned fully, got {checkpoint:?}");
        let indices = provider.block_body_indices(3).expect("read").expect("body indices");
        let stored = provider.senders_by_tx_range(indices.first_tx_num..indices.next_tx_num());
        assert!(
            !matches!(&stored, Ok(senders) if senders.len() as u64 == indices.tx_count),
            "the node holds no senders for block 3, got {stored:?}"
        );
        // Even a range that chose the sender table (no checkpoint yet, or a racing prune)
        // recovers when the table is short.
        let export = ArchiveApiImpl::new(
            provider.clone(),
            ArchiveExportLimits {
                max_range_blocks: DEFAULT_MAX_RANGE_BLOCKS,
                max_range_bytes: DEFAULT_MAX_RANGE_BYTES,
                max_concurrent_ranges: DEFAULT_MAX_CONCURRENT_RANGES,
            },
        );
        let block = export.block_data(3, SenderSource::Table).expect("block 3 exports");
        assert_eq!(block.senders, expected[2], "a short sender table falls back to recovery");
        let secret = JwtSecret::from_hex(DEFAULT_JWT_SECRET.to_string()).expect("test jwt secret");
        let source = RpcChainSource::with_jwt(harness.auth_url(), Duration::from_secs(10), secret)
            .expect("source");
        let status = source.status().await.expect("archive_status");
        assert_eq!(status.earliest_local, 0, "pruned senders do not move the earliest local block");
        let range = source.get_block_range(1, ACTIVE_BLOCKS).await.expect("range is exported");
        let senders: Vec<Vec<Address>> = range.iter().map(|block| block.senders.clone()).collect();
        assert_eq!(senders, expected, "recovered senders equal the senders of each transaction");
        let above = checkpoint.block_number.expect("checkpoint block") + 1;
        assert!(above <= ACTIVE_BLOCKS, "block {above} is above the checkpoint and active");
        let range = source.get_block_range(above, ACTIVE_BLOCKS).await.expect("range is exported");
        let senders: Vec<Vec<Address>> = range.iter().map(|block| block.senders.clone()).collect();
        assert_eq!(
            senders,
            expected[above as usize - 1..],
            "a range above the full-mode checkpoint's block is recovered too"
        );

        let writer = archive_writer(Arc::clone(&store), &chain_spec, &harness).await;
        let finalized = status.finalized;
        assert_eq!(
            drain(&writer).await,
            (finalized + 1) / BLOCKS_PER_SEGMENT,
            "the writer verifies and publishes every segment ending at or below finalized \
             block {finalized}"
        );
    })
    .await
    .expect("e2e run timed out");
}

/// When archive `H` is older than the node's state history, a read of state unchanged since
/// the requested block needs the node's state at `H`, which is pruned. The answer is the
/// EIP-4444 code (`4444`), not an internal error (`-32603`).
#[tokio::test(flavor = "multi_thread")]
async fn archive_behind_node_retention_is_pruned_history_not_internal() {
    tokio::time::timeout(E2E_TIMEOUT, async {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let chain_spec = Arc::new(BaseChainSpec::from_genesis(build_test_genesis()));
        let harness =
            archive_node(Arc::clone(&store), Arc::clone(&chain_spec), short_retention()).await;
        harness.advance_chain(BLOCKS_PER_SEGMENT * 2).await.expect("advance");
        let writer = archive_writer(Arc::clone(&store), &chain_spec, &harness).await;
        let published = drain(&writer).await;
        assert_eq!(published, 2, "segments 0 and 1 end at or below finalized block 15");
        // The node moves on without the writer until its history no longer reaches H.
        while harness.latest_block().number < TIP {
            harness.advance_chain(BLOCKS_PER_SEGMENT).await.expect("advance");
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
        let client = harness.rpc_client().expect("rpc client");
        let untouched = Address::with_last_byte(0x77);
        let err = client
            .request::<_, Value>("eth_getBalance", json!([untouched, "0x1"]))
            .await
            .expect_err("state at H is pruned on the node");
        assert_eq!(
            err.as_error_resp().map(|payload| payload.code),
            Some(PRUNED_HISTORY),
            "an archive lagging the node's retention is pruned history, got {err}"
        );
    })
    .await
    .expect("e2e run timed out");
}

/// On a node that pruned nothing, misses (an unmined receipt, an unknown hash, a block above
/// the head) are answered locally without any archive storage call.
#[tokio::test(flavor = "multi_thread")]
async fn unpruned_node_misses_never_reach_the_archive() {
    tokio::time::timeout(E2E_TIMEOUT, async {
        let gated = GatedStore::new(Arc::new(InMemory::new()));
        let chain_spec = Arc::new(BaseChainSpec::from_genesis(build_test_genesis()));
        let harness =
            archive_node(Arc::new(gated.clone()), chain_spec, LocalNodeRetention::default()).await;
        harness.advance_chain(2).await.expect("advance");
        let client = harness.rpc_client().expect("rpc client");
        let mut gates = archive_lookup_gates(&gated);
        let unknown = B256::repeat_byte(0x42);
        let head = harness.latest_block().number;
        for (method, params) in [
            ("eth_getTransactionReceipt", json!([unknown])),
            ("eth_getTransactionByHash", json!([unknown])),
            ("eth_getBlockByHash", json!([unknown, false])),
            ("eth_getBlockByNumber", json!([format!("0x{:x}", head + 1), false])),
        ] {
            let value = call(&client, method, params.clone()).await;
            assert!(value.is_null(), "{method} {params} must be null locally, got {value}");
        }
        assert!(!gates.any_reached(), "no miss may reach archive storage on an unpruned node");
    })
    .await
    .expect("test timed out");
}

/// Startup identity check: a reachable catalog for another chain fails node start; storage
/// that cannot be reached logs a warning, the node starts, and local reads are served.
#[tokio::test(flavor = "multi_thread")]
async fn startup_refuses_foreign_catalog_and_tolerates_unreachable_storage() {
    tokio::time::timeout(E2E_TIMEOUT, async {
        let config = |store: Arc<dyn ObjectStore>| ArchiveEthConfig {
            source: ArchiveSource { store, layout: ArchiveLayout::new("chain") },
            cache: CacheConfig::default(),
            index: test_index_options(),
            startup_check_timeout: Duration::from_secs(5),
            snapshot_ttl: Duration::ZERO,
        };
        let start = |store: Arc<dyn ObjectStore>| {
            TestHarness::builder()
                .with_chain_spec(Arc::new(BaseChainSpec::from_genesis(build_test_genesis())))
                .with_ext::<ArchiveEthExtension>(Some(config(store)))
                .with_ext::<Eip8130RpcExtension>(TransactionCountOwner::Archive)
                .build()
        };

        let foreign: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        CatalogStore::new(Arc::clone(&foreign), ArchiveLayout::new("chain"))
            .init(&Catalog::new(1, B256::with_last_byte(1), BLOCKS_PER_SEGMENT))
            .await
            .expect("foreign catalog");
        let err = start(foreign).await.expect_err("a foreign catalog must fail node start");
        assert!(
            format!("{err:#}").contains("refusing foreign archive: catalog chain_id mismatch"),
            "start fails on the chain id mismatch, got {err:#}"
        );

        let unreachable: Arc<dyn ObjectStore> = Arc::new(
            AmazonS3Builder::new()
                .with_bucket_name("history")
                .with_region("us-east-1")
                .with_access_key_id("unused")
                .with_secret_access_key("unused")
                .with_endpoint("http://127.0.0.1:1")
                .with_allow_http(true)
                .with_retry(RetryConfig { max_retries: 0, ..Default::default() })
                .build()
                .expect("s3 client"),
        );
        let harness = start(unreachable).await.expect("unreachable storage must not fail start");
        let client = harness.rpc_client().expect("rpc client");
        let genesis = call(&client, "eth_getBlockByNumber", json!(["0x0", false])).await;
        assert!(genesis["hash"].is_string(), "local reads are served, got {genesis}");
    })
    .await
    .expect("test timed out");
}

/// Seeds `nonces[alice][7] = 42` into the EIP-8130 nonce manager, as the EIP-8130 RPC test does.
fn genesis_with_channel_nonce() -> Genesis {
    let slot = NonceManagerStorage::nonce_slot(Account::Alice.address(), U256::from(7u64))
        .expect("non-protocol nonce key");
    let mut genesis = build_test_genesis_everest();
    genesis.alloc.insert(
        NonceManagerStorage::ADDRESS,
        GenesisAccount {
            nonce: Some(1),
            storage: Some(BTreeMap::from([(B256::from(slot), B256::from(U256::from(42u64)))])),
            ..Default::default()
        },
    );
    genesis
}

/// Starts a node with the archive override on (`archive`) or only the EIP-8130 override.
async fn node(genesis: Genesis, archive: bool) -> (TestHarness, RpcClient) {
    let mut builder =
        TestHarness::builder().with_chain_spec(Arc::new(BaseChainSpec::from_genesis(genesis)));
    builder = if archive {
        builder
            .with_ext::<ArchiveEthExtension>(Some(ArchiveEthConfig {
                source: ArchiveSource {
                    store: Arc::new(InMemory::new()),
                    layout: ArchiveLayout::new("chain"),
                },
                cache: CacheConfig::default(),
                index: test_index_options(),
                startup_check_timeout: DEFAULT_STARTUP_CHECK_TIMEOUT,
                snapshot_ttl: Duration::ZERO,
            }))
            .with_ext::<Eip8130RpcExtension>(TransactionCountOwner::Archive)
    } else {
        builder.with_ext::<Eip8130RpcExtension>(TransactionCountOwner::Eip8130)
    };
    let harness = builder.build().await.expect("node starts");
    let client = harness.rpc_client().expect("rpc client");
    (harness, client)
}

/// With the archive on, a non-zero `nonce_key` is answered exactly as the EIP-8130 override
/// answers it: the seeded channel value after Everest, the same error before.
#[tokio::test(flavor = "multi_thread")]
async fn nonzero_nonce_key_matches_eip8130() {
    tokio::time::timeout(E2E_TIMEOUT, async {
        let alice = Account::Alice.address();
        let params = json!([alice, "latest", "0x7"]);
        for (genesis, label) in [
            (genesis_with_channel_nonce as fn() -> Genesis, "everest"),
            (build_test_genesis_cobalt as fn() -> Genesis, "pre-everest"),
        ] {
            let (_archive_node, archive) = node(genesis(), true).await;
            let (_plain_node, plain) = node(genesis(), false).await;
            let with_archive = call(&archive, "eth_getTransactionCount", params.clone()).await;
            let eip8130 = call(&plain, "eth_getTransactionCount", params.clone()).await;
            assert_eq!(
                with_archive, eip8130,
                "{label}: archive override must delegate to EIP-8130"
            );
            if label == "everest" {
                assert_eq!(with_archive, json!("0x2a"), "everest: seeded channel nonce 42");
            } else {
                assert!(
                    with_archive["error"].as_str().is_some_and(|err| err.contains("-32602")),
                    "pre-everest: INVALID_PARAMS expected, got {with_archive}"
                );
            }
            let legacy = call(&archive, "eth_getTransactionCount", json!([alice, "latest"])).await;
            let zero =
                call(&archive, "eth_getTransactionCount", json!([alice, "latest", "0x0"])).await;
            assert_eq!(legacy, zero, "{label}: nonce_key 0 equals the protocol nonce");
        }
    })
    .await
    .expect("test timed out");
}
