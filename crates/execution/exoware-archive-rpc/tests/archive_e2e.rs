//! End-to-end check of the Exoware archive on a short-retention dev node.
//!
//! One node runs the exporter against an in-process Exoware simulator, prunes bodies, receipts
//! and state history older than [`RETENTION`] blocks, and answers `eth_` reads with the
//! archive-backed override. Responses captured while the node still held the history must
//! equal, as JSON, the responses the pruned node serves later.

use std::{collections::BTreeMap, sync::Arc, time::Duration};

use alloy_consensus::Sealed;
use alloy_eips::{Decodable2718, Encodable2718};
use alloy_genesis::{Genesis, GenesisAccount};
use alloy_primitives::{Address, B256, Bytes, U256};
use alloy_rpc_client::RpcClient;
use base_common_consensus::BaseTxEnvelope;
use base_common_precompiles::NonceManagerStorage;
use base_common_rpc_types::BaseTransactionRequest;
use base_execution_base_tx_rpc_node::TxBaseRpcExtension;
use base_execution_chainspec::BaseChainSpec;
use base_exoware_archive::{ArchiveKey, ArchiveStore, BlockVerifier, ExowareArchive, ExowareStore};
use base_exoware_archive_rpc::{ExowareArchiveArgs, ExowareArchiveExtension};
use base_node_runner::test_utils::{L1_BLOCK_INFO_DEPOSIT_TX, LocalNodeRetention, TestHarness};
use base_protocol::BaseTimeUpdateTx;
use base_test_utils::{
    Account, build_test_genesis, build_test_genesis_cobalt, build_test_genesis_everest,
};
use reth_chainspec::EthChainSpec;
use reth_node_core::args::{PruningArgs, StaticFilesArgs};
use reth_prune_types::PruneSegment;
use reth_storage_api::{
    BlockNumReader, BlockReader, PruneCheckpointReader, StateProviderFactory, errors::ProviderError,
};
use serde_json::{Value, json};

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
/// Upper bound for one end-to-end run.
const E2E_TIMEOUT: Duration = Duration::from_secs(300);
/// Upper bound for the exporter to catch up with the finalized head.
const EXPORT_TIMEOUT: Duration = Duration::from_secs(60);
/// Archive namespace used by the tests.
const NAMESPACE: &str = "e2e-archive/";

/// Runtime code: `sstore(0, calldataload(0)); log1(0, 0, calldataload(0))`.
const RUNTIME: [u8; 13] =
    [0x60, 0x00, 0x35, 0x80, 0x60, 0x00, 0x55, 0x60, 0x00, 0x60, 0x00, 0xa1, 0x00];

/// Init code returning [`RUNTIME`].
fn init_code() -> Bytes {
    let mut code = vec![0x60, 0x0d, 0x60, 0x0c, 0x60, 0x00, 0x39, 0x60, 0x0d, 0x60, 0x00, 0xf3];
    code.extend_from_slice(&RUNTIME);
    code.into()
}

/// The harness's L1 info deposit with a per-block source hash, so hash-keyed receipt lookups
/// before pruning are well defined.
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

/// A node that keeps only the last [`RETENTION`] blocks of history.
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

/// Archive arguments for a store at `url`.
fn archive_args(url: &str, export: bool) -> ExowareArchiveArgs {
    ExowareArchiveArgs {
        archive_exoware_url: Some(url.to_string()),
        archive_exoware_namespace: NAMESPACE.to_string(),
        archive_exoware_export: export,
        archive_exoware_provision: export,
        archive_exoware_segment_blocks: BLOCKS_PER_SEGMENT,
        archive_exoware_timeout: Duration::from_secs(2),
        ..ExowareArchiveArgs::default()
    }
}

/// Starts a node with the Base transaction override, then the archive extension.
async fn archive_node(
    chain_spec: Arc<BaseChainSpec>,
    retention: LocalNodeRetention,
    args: ExowareArchiveArgs,
    max_logs: Option<u64>,
) -> TestHarness {
    let mut builder = TestHarness::builder().with_chain_spec(chain_spec).with_retention(retention);
    if let Some(max) = max_logs {
        builder = builder.with_max_logs_per_response(max);
    }
    builder
        .with_ext::<TxBaseRpcExtension>(())
        .with_ext::<ExowareArchiveExtension>(args)
        .build()
        .await
        .expect("node starts")
}

/// Sends one JSON-RPC request and returns the result, or the error code and message as JSON.
async fn call(client: &RpcClient, method: &'static str, params: Value) -> Value {
    match client.request::<_, Value>(method, params.clone()).await {
        Ok(value) => value,
        Err(err) => err.as_error_resp().map_or_else(
            || json!({ "transport": err.to_string(), "method": method, "params": params }),
            |payload| json!({ "code": payload.code, "message": payload.message }),
        ),
    }
}

/// Number-addressed requests over the active blocks. Multi-block log queries are left out
/// when `with_range_logs` is false (a node with a tiny log cap refuses them before pruning too).
fn number_requests(
    blocks: &[(u64, B256)],
    contract: Address,
    with_range_logs: bool,
) -> Vec<(&'static str, Value)> {
    let mut out = Vec::new();
    for (number, hash) in blocks {
        let tag = format!("0x{number:x}");
        out.push(("eth_getBlockByNumber", json!([tag, true])));
        out.push(("eth_getBlockByNumber", json!([tag, false])));
        out.push(("eth_getBlockTransactionCountByNumber", json!([tag])));
        if *number <= ACTIVE_BLOCKS {
            out.push(("eth_getBlockReceipts", json!([tag])));
            out.push(("eth_getTransactionByBlockNumberAndIndex", json!([tag, "0x1"])));
        }
        for account in [Account::Alice.address(), Account::Bob.address(), contract] {
            out.push(("eth_getBalance", json!([account, tag])));
            out.push(("eth_getTransactionCount", json!([account, tag])));
            out.push(("eth_getCode", json!([account, tag])));
        }
        out.push(("eth_getTransactionCount", json!([Account::Alice.address(), tag, "0x0"])));
        out.push(("eth_getStorageAt", json!([contract, "0x0", tag])));
        out.push(("eth_getBalance", json!([Account::Alice.address(), { "blockHash": hash }])));
        out.push((
            "eth_getStorageAt",
            json!([contract, "0x0", { "blockHash": hash, "requireCanonical": true }]),
        ));
        out.push((
            "eth_getTransactionCount",
            json!([Account::Alice.address(), { "blockNumber": tag }]),
        ));
    }
    if with_range_logs {
        let last = format!("0x{:x}", blocks.last().expect("blocks").0);
        out.push(("eth_getLogs", json!([{ "fromBlock": "0x1", "toBlock": last }])));
        out.push((
            "eth_getLogs",
            json!([{ "fromBlock": "0x1", "toBlock": last, "address": contract }]),
        ));
    }
    out
}

/// Hash-addressed requests over the active blocks and transactions.
fn hash_requests(blocks: &[(u64, B256)], tx_hashes: &[B256]) -> Vec<(&'static str, Value)> {
    let mut out = Vec::new();
    for (number, hash) in blocks {
        out.push(("eth_getBlockByHash", json!([hash, true])));
        out.push(("eth_getBlockByHash", json!([hash, false])));
        out.push(("eth_getBlockTransactionCountByHash", json!([hash])));
        if *number <= ACTIVE_BLOCKS {
            out.push(("eth_getTransactionByBlockHashAndIndex", json!([hash, "0x2"])));
            out.push(("eth_getBlockReceipts", json!([{ "blockHash": hash }])));
            out.push(("eth_getLogs", json!([{ "blockHash": hash }])));
        }
    }
    for hash in tx_hashes {
        out.push(("eth_getTransactionByHash", json!([hash])));
        out.push(("eth_getTransactionReceipt", json!([hash])));
    }
    out
}

/// Waits until the archive's highest marker is at least `segment`.
async fn wait_for_segment(store: &ExowareStore, segment: u64) {
    tokio::time::timeout(EXPORT_TIMEOUT, async {
        loop {
            let highest = ExowareArchive::<ExowareStore>::highest_marker(&store.session())
                .await
                .expect("marker read");
            if highest.is_some_and(|h| h.segment >= segment) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("exporter did not publish segment {segment} in time"));
}

/// One retention setup to check for parity.
struct Scenario {
    /// Name for assertion messages.
    name: &'static str,
    /// Node retention.
    retention: LocalNodeRetention,
    /// Prune segment whose checkpoint must cover every compared block.
    pruned: PruneSegment,
    /// `--rpc.max-logs-per-response`.
    max_logs: Option<u64>,
    /// Run the checks that need bodies and state pruned too (warm-cache prune, tags, forged
    /// locators, store down).
    full: bool,
}

/// A pruned node serves every covered `eth_` method from the archive with JSON parity, also
/// for a block pruned while the caches were warm. Tags stay local, forged locators are 4444,
/// and with the store down number-addressed reads in the pruned range are 4444 and an unknown
/// hash is `null`.
#[tokio::test(flavor = "multi_thread")]
async fn pruned_node_serves_history_from_archive() {
    let scenario = Scenario {
        name: "full",
        retention: short_retention(),
        pruned: PruneSegment::Bodies,
        max_logs: None,
        full: true,
    };
    tokio::time::timeout(E2E_TIMEOUT, pruned_node_parity(scenario))
        .await
        .expect("e2e run timed out");
}

/// Receipt-only pruning: bodies and state stay local, but every answer with deposit fields
/// and every log comes from the archive.
#[tokio::test(flavor = "multi_thread")]
async fn receipt_only_pruning_serves_deposit_fields_from_archive() {
    let retention = LocalNodeRetention {
        pruning: PruningArgs {
            block_interval: Some(1),
            minimum_distance: Some(RETENTION),
            receipts_distance: Some(RETENTION),
            ..Default::default()
        },
        static_files: short_retention().static_files,
    };
    let scenario = Scenario {
        name: "receipts",
        retention,
        pruned: PruneSegment::Receipts,
        max_logs: None,
        full: false,
    };
    tokio::time::timeout(E2E_TIMEOUT, pruned_node_parity(scenario))
        .await
        .expect("e2e run timed out");
}

/// Transaction-lookup-only pruning: the node cannot place a transaction hash, and Base would
/// default deposit fields; hash reads resolve through the archive's locators.
#[tokio::test(flavor = "multi_thread")]
async fn lookup_only_pruning_resolves_hashes_in_archive() {
    let retention = LocalNodeRetention {
        pruning: PruningArgs {
            block_interval: Some(1),
            transaction_lookup_distance: Some(RETENTION),
            ..Default::default()
        },
        static_files: short_retention().static_files,
    };
    let scenario = Scenario {
        name: "lookup",
        retention,
        pruned: PruneSegment::TransactionLookup,
        max_logs: None,
        full: false,
    };
    tokio::time::timeout(E2E_TIMEOUT, pruned_node_parity(scenario))
        .await
        .expect("e2e run timed out");
}

/// Unequal retention: account history is kept but storage history is pruned. reth refuses
/// account reads at old blocks as pruned (or with `InsufficientChangesets`) although the
/// account checkpoint says they are local; those refusals go to the archive.
#[tokio::test(flavor = "multi_thread")]
async fn asymmetric_state_retention_serves_refused_state_from_archive() {
    let mut retention = short_retention();
    retention.pruning.account_history_distance = None;
    let scenario = Scenario {
        name: "asymmetric",
        retention,
        pruned: PruneSegment::StorageHistory,
        max_logs: None,
        full: false,
    };
    tokio::time::timeout(E2E_TIMEOUT, pruned_node_parity(scenario))
        .await
        .expect("e2e run timed out");
}

/// The node's `--rpc.max-logs-per-response` applies to the combined archive + local result
/// with reth's error, whether the crossing block is archived or local.
#[tokio::test(flavor = "multi_thread")]
async fn combined_log_limit_matches_reth() {
    let scenario = Scenario {
        name: "limits",
        retention: short_retention(),
        pruned: PruneSegment::Receipts,
        max_logs: Some(2),
        full: false,
    };
    tokio::time::timeout(E2E_TIMEOUT, pruned_node_parity(scenario))
        .await
        .expect("e2e run timed out");
}

/// A TCP listener standing in for the store that counts connections and drops them, so any
/// store call shows up in the count.
async fn counting_store() -> (String, Arc<std::sync::atomic::AtomicUsize>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let url = format!("http://{}", listener.local_addr().expect("addr"));
    let connections = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = Arc::clone(&connections);
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            drop(stream);
        }
    });
    (url, connections)
}

/// P2: pending-transaction polling never reaches the store. On a node whose transaction
/// lookup is pruned, a pooled hash answers `null` with zero store connections while an
/// unknown hash does consult the store; on an unpruned node an unknown hash makes zero store
/// connections. (A lookup-pruned node still resolves archived hashes through the store:
/// `lookup_only_pruning_resolves_hashes_in_archive`.)
#[tokio::test(flavor = "multi_thread")]
async fn pending_hash_polling_skips_the_store() {
    tokio::time::timeout(E2E_TIMEOUT, async {
        let lookup_pruned = LocalNodeRetention {
            pruning: PruningArgs {
                block_interval: Some(1),
                transaction_lookup_distance: Some(RETENTION),
                ..Default::default()
            },
            static_files: short_retention().static_files,
        };
        for (name, retention, pruned) in [
            ("lookup-pruned", lookup_pruned, true),
            ("unpruned", LocalNodeRetention::default(), false),
        ] {
            let (url, connections) = counting_store().await;
            let chain_spec = Arc::new(BaseChainSpec::from_genesis(build_test_genesis()));
            let harness =
                archive_node(chain_spec, retention, archive_args(&url, false), None).await;
            let client = harness.rpc_client().expect("rpc client");
            build_active_blocks(&harness).await;
            if pruned {
                harness.advance_chain(RETENTION + 2 * BLOCKS_PER_SEGMENT).await.expect("advance");
                let checkpoint = harness
                    .blockchain_provider()
                    .get_prune_checkpoint(PruneSegment::TransactionLookup)
                    .expect("checkpoint read")
                    .and_then(|c| c.block_number);
                assert!(checkpoint.is_some(), "{name}: the transaction lookup is pruned");
            }
            let (pending, pending_hash) = Account::Alice
                .sign_txn_request(
                    BaseTransactionRequest::default()
                        .to(Account::Bob.address())
                        .value(U256::from(1))
                        .nonce(ACTIVE_BLOCKS * 2),
                )
                .expect("pending tx");
            let sent = call(&client, "eth_sendRawTransaction", json!([pending])).await;
            assert_eq!(sent, json!(pending_hash), "{name}: the transaction enters the pool");
            for _ in 0..3 {
                for method in ["eth_getTransactionReceipt", "eth_getTransactionByHash"] {
                    let value = call(&client, method, json!([pending_hash])).await;
                    if method == "eth_getTransactionReceipt" {
                        assert_eq!(value, Value::Null, "{name}: a pending receipt is null");
                    } else {
                        assert_eq!(
                            value["hash"],
                            json!(pending_hash),
                            "{name}: the pending transaction is served by the node"
                        );
                    }
                }
            }
            assert_eq!(
                connections.load(std::sync::atomic::Ordering::SeqCst),
                0,
                "{name}: polling a pooled hash makes zero store connections"
            );
            let unknown = B256::repeat_byte(0x42);
            let value = call(&client, "eth_getTransactionReceipt", json!([unknown])).await;
            assert_eq!(value, Value::Null, "{name}: an unknown hash is null");
            let expected = usize::from(pruned);
            let seen = connections.load(std::sync::atomic::Ordering::SeqCst);
            if pruned {
                assert!(
                    seen >= expected,
                    "{name}: an unplaced hash on a lookup-pruned node consults the store ({seen})"
                );
            } else {
                assert_eq!(
                    seen, 0,
                    "{name}: an unpruned lookup answers an unknown hash with zero store connections"
                );
            }
        }
    })
    .await
    .expect("test timed out");
}

/// Forged locators: an unknown block hash whose locator points at block 2 resolves in the
/// archive, then fails the hash check, and is 4444 rather than a wrong block or null. A forged
/// transaction locator is only consulted when the node's transaction lookup is pruned
/// (`lookup_pruned`); otherwise the node's `null` stands and the store is not asked.
async fn check_forged_locators(
    client: &RpcClient,
    store: &ExowareStore,
    name: &str,
    lookup_pruned: bool,
) {
    let forged_block = B256::repeat_byte(0xb1);
    let forged_tx = B256::repeat_byte(0xc1);
    let mut tx_locator = 2u64.to_be_bytes().to_vec();
    tx_locator.extend(0u32.to_be_bytes());
    store
        .put(&[
            (ArchiveKey::block_hash(forged_block, 0), 2u64.to_be_bytes().to_vec().into()),
            (ArchiveKey::tx_hash(forged_tx, 0), tx_locator.into()),
        ])
        .await
        .expect("forge locators");
    for (method, params) in [
        ("eth_getBlockByHash", json!([forged_block, true])),
        ("eth_getBlockTransactionCountByHash", json!([forged_block])),
        ("eth_getLogs", json!([{ "blockHash": forged_block }])),
    ] {
        let value = call(client, method, params.clone()).await;
        assert_eq!(
            value["code"],
            json!(PRUNED_HISTORY),
            "{name}: {method} {params} with a forged locator is 4444, got {value}"
        );
    }
    for (method, params) in [
        ("eth_getTransactionByHash", json!([forged_tx])),
        ("eth_getTransactionReceipt", json!([forged_tx])),
    ] {
        let value = call(client, method, params.clone()).await;
        if lookup_pruned {
            assert_eq!(
                value["code"],
                json!(PRUNED_HISTORY),
                "{name}: {method} {params} with a forged locator is 4444, got {value}"
            );
        } else {
            assert_eq!(
                value,
                Value::Null,
                "{name}: {method} {params} stays null while the lookup is unpruned"
            );
        }
    }
}

/// Builds `ACTIVE_BLOCKS` blocks of transfers and contract calls on `harness`; returns the
/// contract and every transaction hash.
async fn build_active_blocks(harness: &TestHarness) -> (Address, Vec<B256>) {
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
        let (set, set_hash) = contract_call(contract, 100 + number, nonce * 2 + 1);
        txs.extend([transfer, set]);
        tx_hashes.extend([transfer_hash, set_hash]);
        harness.build_block_from_transactions(txs).await.expect("active block");
    }
    (contract, tx_hashes)
}

/// Alice's call storing and logging `value` in the contract.
fn contract_call(contract: Address, value: u64, nonce: u64) -> (Bytes, B256) {
    Account::Alice
        .sign_txn_request(
            BaseTransactionRequest::default()
                .to(contract)
                .input(B256::from(U256::from(value)).0.to_vec().into())
                .nonce(nonce),
        )
        .expect("contract call tx")
}

/// Body of the parity tests.
async fn pruned_node_parity(scenario: Scenario) {
    let name = scenario.name;
    let (simulator, url) = exoware_simulator::open_temp().await.expect("simulator starts");
    let store = ExowareStore::connect(&url, NAMESPACE, Duration::from_secs(5)).expect("store");
    let chain_spec = Arc::new(BaseChainSpec::from_genesis(build_test_genesis()));
    let harness =
        archive_node(chain_spec, scenario.retention, archive_args(&url, true), scenario.max_logs)
            .await;
    let client = harness.rpc_client().expect("rpc client");
    let (contract, tx_hashes) = build_active_blocks(&harness).await;
    harness.advance_chain(BLOCKS_PER_SEGMENT * 2 - ACTIVE_BLOCKS).await.expect("advance");

    let provider = harness.blockchain_provider();
    let blocks: Vec<(u64, B256)> = (1..=ACTIVE_BLOCKS + 2)
        .map(|n| {
            let block = provider.block_by_number(n).expect("read").expect("block exists");
            (n, alloy_primitives::Sealable::hash_slow(&block.header))
        })
        .collect();
    let requests: Vec<_> = number_requests(&blocks, contract, scenario.max_logs.is_none())
        .into_iter()
        .chain(hash_requests(&blocks, &tx_hashes))
        .collect();
    let mut expected = Vec::with_capacity(requests.len());
    for (method, params) in &requests {
        expected.push(call(&client, method, params.clone()).await);
    }
    for ((method, params), value) in requests.iter().zip(&expected) {
        assert!(
            value.get("code").is_none() && value.get("transport").is_none() && !value.is_null(),
            "{name}: {method} {params} must succeed before pruning, got {value}"
        );
    }
    let active_logs = call(
        &client,
        "eth_getLogs",
        json!([{ "fromBlock": "0x1", "toBlock": format!("0x{ACTIVE_BLOCKS:x}"), "address": contract }]),
    )
    .await;
    let active_logs = active_logs.as_array().cloned().unwrap_or_else(|| {
        // A log cap below the active log count refuses this before pruning; rebuild it per block.
        Vec::new()
    });
    let mut per_block_logs = Vec::new();
    for (number, hash) in &blocks[..ACTIVE_BLOCKS as usize] {
        let logs =
            call(&client, "eth_getLogs", json!([{ "blockHash": hash, "address": contract }])).await;
        let logs = logs
            .as_array()
            .cloned()
            .unwrap_or_else(|| panic!("{name}: logs of block {number}: {logs}"));
        assert_eq!(logs.len(), 1, "{name}: block {number} has one contract log");
        per_block_logs.extend(logs);
    }
    if scenario.max_logs.is_none() {
        assert_eq!(
            active_logs, per_block_logs,
            "{name}: range and per-block logs agree before pruning"
        );
    }
    let warm_block = blocks[0];
    let warm_requests = [
        ("eth_getBlockByNumber", json!(["0x1", true])),
        ("eth_getTransactionReceipt", json!([tx_hashes[1]])),
    ];
    let warm_expected = [
        call(&client, warm_requests[0].0, warm_requests[0].1.clone()).await,
        call(&client, warm_requests[1].0, warm_requests[1].1.clone()).await,
    ];

    // Export while the chain grows past the retention window: the finalized head trails the
    // head by one block, so segment k is published once block 8k+8 exists.
    wait_for_segment(&store, 0).await;
    let mut warm_checked = false;
    while harness.latest_block().number < TIP {
        let head = harness.latest_block().number;
        if scenario.full && !warm_checked && head + BLOCKS_PER_SEGMENT > RETENTION + 1 {
            // Warm-cache prune: a background reader keeps the node's block cache and the
            // boundary's retention cache warm while the node prunes block 1, so the re-read
            // right after the prune meets caches filled before it. (A wall-clock bound on the
            // cache age is not asserted: block building under parallel tests can exceed the
            // retention TTL; the answer must be exact either way.)
            let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let warmer = {
                let (client, stop, requests) =
                    (client.clone(), Arc::clone(&stop), warm_requests.clone());
                tokio::spawn(async move {
                    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                        for (method, params) in &requests {
                            call(&client, method, params.clone()).await;
                        }
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                })
            };
            let mut pruned = false;
            for _ in 0..(4 * BLOCKS_PER_SEGMENT) {
                harness.advance_chain(1).await.expect("advance");
                if matches!(
                    provider.block_by_number(warm_block.0),
                    Err(ProviderError::BlockExpired { requested: 1, .. })
                ) {
                    pruned = true;
                    break;
                }
            }
            let mut after = Vec::new();
            for (method, params) in &warm_requests {
                after.push(call(&client, method, params.clone()).await);
            }
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
            warmer.await.expect("warm reader");
            assert!(pruned, "{name}: block 1 is pruned within {} blocks", 4 * BLOCKS_PER_SEGMENT);
            assert_eq!(
                after, warm_expected,
                "{name}: a block pruned under warm caches keeps its deposit fields"
            );
            warm_checked = true;
            let head = harness.latest_block().number;
            wait_for_segment(&store, head / BLOCKS_PER_SEGMENT - 2).await;
            continue;
        }
        harness.advance_chain(BLOCKS_PER_SEGMENT).await.expect("advance");
        let head = harness.latest_block().number;
        wait_for_segment(&store, head / BLOCKS_PER_SEGMENT - 2).await;
    }
    assert_eq!(warm_checked, scenario.full, "{name}: warm-cache check ran exactly when expected");

    // A retained block with a matching log, so log queries span both sides.
    let head = harness.latest_block().number;
    let (retained_call, _) = contract_call(contract, 999, ACTIVE_BLOCKS * 2);
    harness
        .build_block_from_transactions(vec![
            l1_info_deposit(head + 1),
            base_time_deposit(head + 1),
            retained_call,
        ])
        .await
        .expect("retained block");
    let retained = head + 1;
    let retained_hash = alloy_primitives::Sealable::hash_slow(
        &provider.block_by_number(retained).expect("read").expect("retained block").header,
    );
    let retained_logs =
        call(&client, "eth_getLogs", json!([{ "blockHash": retained_hash, "address": contract }]))
            .await;
    let retained_logs = retained_logs.as_array().cloned().expect("retained logs");
    assert_eq!(retained_logs.len(), 1, "{name}: the retained block has one contract log");

    // The node no longer has the compared blocks' data for the scenario's segment.
    let checkpoint = provider
        .get_prune_checkpoint(scenario.pruned)
        .expect("checkpoint read")
        .and_then(|c| c.block_number);
    // Blocks after the active ones hold no transactions, so the lookup checkpoint stops at
    // the last active block.
    let through = if scenario.pruned == PruneSegment::TransactionLookup {
        ACTIVE_BLOCKS
    } else {
        ACTIVE_BLOCKS + 2
    };
    assert!(
        checkpoint.is_some_and(|c| c >= through),
        "{name}: {:?} is pruned through every compared block, checkpoint {checkpoint:?}",
        scenario.pruned
    );
    if scenario.full {
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
        let earliest = provider.earliest_block_number().expect("earliest block");
        let pruned_body = provider.block_by_number(1);
        assert!(
            matches!(pruned_body, Err(ProviderError::BlockExpired { requested: 1, earliest_available }) if earliest_available == earliest),
            "the body of block 1 reads as exactly BlockExpired {{ 1, {earliest} }}, got {pruned_body:?}"
        );
        assert!(
            earliest > ACTIVE_BLOCKS + 2,
            "every compared block is pruned (earliest {earliest})"
        );
    }

    for ((method, params), before) in requests.iter().zip(&expected) {
        let after = call(&client, method, params.clone()).await;
        assert_eq!(&after, before, "{name}: {method} {params} must match the pre-pruning response");
    }

    // Split logs: archived and retained matches, in order, with no gap.
    let combined = json!([{ "fromBlock": "0x1", "toBlock": "latest", "address": contract }]);
    let split = call(&client, "eth_getLogs", combined.clone()).await;
    match scenario.max_logs {
        None => {
            let expected: Vec<Value> =
                per_block_logs.iter().chain(&retained_logs).cloned().collect();
            assert_eq!(
                split,
                Value::Array(expected),
                "{name}: archived then retained logs, exactly"
            );
        }
        Some(max) => {
            assert_eq!(
                split,
                json!({ "code": -32602, "message": format!("query exceeds max results {max}, retry with the range 1-2") }),
                "{name}: the cap fails at archived block 3"
            );
            let tail = call(
                &client,
                "eth_getLogs",
                json!([{ "fromBlock": format!("0x{:x}", ACTIVE_BLOCKS - 1), "toBlock": "latest", "address": contract }]),
            )
            .await;
            assert_eq!(
                tail,
                json!({
                    "code": -32602,
                    "message": format!(
                        "query exceeds max results {max}, retry with the range {}-{}",
                        ACTIVE_BLOCKS - 1,
                        retained - 1
                    )
                }),
                "{name}: the cap fails at the retained block after two archived logs"
            );
            let within = call(
                &client,
                "eth_getLogs",
                json!([{ "fromBlock": format!("0x{ACTIVE_BLOCKS:x}"), "toBlock": "latest", "address": contract }]),
            )
            .await;
            let expected: Vec<Value> = per_block_logs[ACTIVE_BLOCKS as usize - 1..]
                .iter()
                .chain(&retained_logs)
                .cloned()
                .collect();
            assert_eq!(
                within,
                Value::Array(expected),
                "{name}: exactly the cap is served across the split"
            );
        }
    }
    if scenario.pruned == PruneSegment::TransactionLookup {
        check_forged_locators(&client, &store, name, true).await;
    }
    if !scenario.full {
        return;
    }
    check_forged_locators(&client, &store, name, false).await;

    // Tags and pending stay local on a pruned node.
    let head = harness.latest_block().number;
    let alice = Account::Alice.address();
    let at_head = call(&client, "eth_getBalance", json!([alice, format!("0x{head:x}")])).await;
    for tag in ["latest", "pending", "safe", "finalized"] {
        let value = call(&client, "eth_getBalance", json!([alice, tag])).await;
        assert!(value.is_string(), "{tag} balance is served locally, got {value}");
        if tag != "safe" && tag != "finalized" {
            assert_eq!(value, at_head, "{tag} balance equals the balance at the head");
        }
    }
    let latest = call(&client, "eth_getBlockByNumber", json!(["latest", false])).await;
    assert_eq!(latest["number"], json!(format!("0x{head:x}")), "latest is the head");
    let pending_logs =
        call(&client, "eth_getLogs", json!([{ "fromBlock": "latest", "toBlock": "pending" }]))
            .await;
    assert!(
        pending_logs.is_array(),
        "a latest..pending log query is served locally, got {pending_logs}"
    );
    let next =
        call(&client, "eth_getBlockByNumber", json!([format!("0x{:x}", head + 1), false])).await;
    assert_eq!(next, Value::Null, "block {} does not exist yet", head + 1);

    // Store down: number-addressed reads in the pruned range and hashes the node can place
    // are 4444 with a reason, an unknown hash keeps the node's `null`, and recent blocks are
    // still served locally.
    simulator.abort();
    let _ = simulator.await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    for (method, params) in [
        ("eth_getBlockByNumber", json!(["0x2", true])),
        ("eth_getBalance", json!([alice, "0x2"])),
        ("eth_getStorageAt", json!([contract, "0x0", "0x2"])),
        ("eth_getLogs", json!([{ "fromBlock": "0x1", "toBlock": "0x3" }])),
        ("eth_getBlockByHash", json!([blocks[1].1, true])),
    ] {
        let value = call(&client, method, params.clone()).await;
        assert_eq!(
            value["code"],
            json!(PRUNED_HISTORY),
            "{method} {params} is 4444 with the store down, got {value}"
        );
        assert!(
            value["message"].as_str().is_some_and(|m| m.starts_with("pruned history unavailable")),
            "{method} {params} names pruned history, got {value}"
        );
    }
    // A complete local answer (here a non-deposit receipt the node still serves) is never
    // routed to the archive, so it is served exactly as before pruning with the store down.
    let receipt = call(&client, "eth_getTransactionReceipt", json!([tx_hashes[1]])).await;
    assert_eq!(receipt, warm_expected[1], "a complete local receipt is served with the store down");
    let unknown = B256::repeat_byte(0x42);
    for (method, params) in [
        ("eth_getTransactionReceipt", json!([unknown])),
        ("eth_getTransactionByHash", json!([unknown])),
        ("eth_getBlockByHash", json!([unknown, false])),
    ] {
        let value = call(&client, method, params.clone()).await;
        assert_eq!(
            value,
            Value::Null,
            "{method} of an unknown hash stays null with the store down"
        );
    }
    let latest = call(&client, "eth_getBlockByNumber", json!(["latest", false])).await;
    assert_eq!(
        latest["number"],
        json!(format!("0x{head:x}")),
        "recent blocks are served locally with the store down"
    );
}

/// On an unpruned node whose store is unreachable, misses (an unmined receipt, an unknown
/// hash, a block above the head) and recent reads answer locally. An exporter whose shutdown
/// is already signaled does not provision.
#[tokio::test(flavor = "multi_thread")]
async fn unpruned_node_with_store_down_answers_locally() {
    tokio::time::timeout(E2E_TIMEOUT, async {
        let chain_spec = Arc::new(BaseChainSpec::from_genesis(build_test_genesis()));
        let harness = archive_node(
            Arc::clone(&chain_spec),
            LocalNodeRetention::default(),
            archive_args("http://127.0.0.1:1", false),
            None,
        )
        .await;
        harness.advance_chain(2).await.expect("advance");
        let client = harness.rpc_client().expect("rpc client");
        let unknown = B256::repeat_byte(0x42);
        let head = harness.latest_block().number;
        for (method, params) in [
            ("eth_getTransactionReceipt", json!([unknown])),
            ("eth_getTransactionByHash", json!([unknown])),
            ("eth_getBlockByHash", json!([unknown, false])),
            ("eth_getBlockByNumber", json!([format!("0x{:x}", head + 1), false])),
        ] {
            let value = call(&client, method, params.clone()).await;
            assert_eq!(value, Value::Null, "{method} {params} must be a local null");
        }
        let genesis = call(&client, "eth_getBlockByNumber", json!(["0x0", true])).await;
        assert!(genesis["hash"].is_string(), "genesis is served locally, got {genesis}");
        let balance =
            call(&client, "eth_getBalance", json!([Account::Alice.address(), "0x1"])).await;
        assert!(balance.is_string(), "unpruned state is served locally, got {balance}");

        let (simulator, url) = exoware_simulator::open_temp().await.expect("simulator starts");
        let store = Arc::new(
            ExowareStore::connect(&url, NAMESPACE, Duration::from_secs(5)).expect("store"),
        );
        let exported = ExowareArchiveExtension::export(
            Arc::clone(&store),
            harness.blockchain_provider(),
            BlockVerifier::new(Arc::clone(&chain_spec)),
            archive_args(&url, true),
            chain_spec.chain().id(),
            chain_spec.genesis_hash(),
            std::future::ready(()),
        )
        .await;
        assert_eq!(exported, Ok(()), "a signaled exporter exits cleanly");
        let config = ExowareArchive::<ExowareStore>::read_config(&store.session())
            .await
            .expect("config read");
        assert_eq!(config, None, "a signaled exporter does not provision");
        simulator.abort();
    })
    .await
    .expect("test timed out");
}

/// Seeds `nonces[alice][7] = 42` into the nonce manager, as the Base transaction RPC test does.
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

/// With the archive on, `eth_getTransactionCount` answers exactly as the Base transaction
/// override does: the seeded channel value after Everest, the same error before, and the
/// protocol nonce for `nonce_key` 0.
#[tokio::test(flavor = "multi_thread")]
async fn transaction_count_override_is_preserved() {
    tokio::time::timeout(E2E_TIMEOUT, async {
        let alice = Account::Alice.address();
        let params = json!([alice, "latest", "0x7"]);
        for (genesis, label) in [
            (genesis_with_channel_nonce as fn() -> Genesis, "everest"),
            (build_test_genesis_cobalt as fn() -> Genesis, "pre-everest"),
        ] {
            let archive = archive_node(
                Arc::new(BaseChainSpec::from_genesis(genesis())),
                LocalNodeRetention::default(),
                archive_args("http://127.0.0.1:1", false),
                None,
            )
            .await;
            let plain = TestHarness::builder()
                .with_chain_spec(Arc::new(BaseChainSpec::from_genesis(genesis())))
                .with_ext::<TxBaseRpcExtension>(())
                .build()
                .await
                .expect("node starts");
            let archive_client = archive.rpc_client().expect("rpc client");
            let plain_client = plain.rpc_client().expect("rpc client");
            let with_archive =
                call(&archive_client, "eth_getTransactionCount", params.clone()).await;
            let base_tx = call(&plain_client, "eth_getTransactionCount", params.clone()).await;
            assert_eq!(
                with_archive, base_tx,
                "{label}: archive must delegate to the Base override"
            );
            if label == "everest" {
                assert_eq!(with_archive, json!("0x2a"), "everest: seeded channel nonce 42");
            } else {
                assert_eq!(
                    with_archive["code"],
                    json!(-32602),
                    "pre-everest: invalid params, got {with_archive}"
                );
            }
            let legacy =
                call(&archive_client, "eth_getTransactionCount", json!([alice, "latest"])).await;
            let zero =
                call(&archive_client, "eth_getTransactionCount", json!([alice, "latest", "0x0"]))
                    .await;
            let plain_legacy =
                call(&plain_client, "eth_getTransactionCount", json!([alice, "latest"])).await;
            assert_eq!(legacy, zero, "{label}: nonce_key 0 equals the protocol nonce");
            assert_eq!(legacy, plain_legacy, "{label}: protocol nonce matches the plain node");
        }
    })
    .await
    .expect("test timed out");
}
