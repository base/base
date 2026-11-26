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
use base_execution_rpc::{PrunedHistory, PrunedState};
use base_exoware_archive::{
    ArchiveKey, ArchiveStore, BlockCodec, BlockVerifier, ExowareArchive, ExowareStore, Family,
    ReaderLimits, StateKey, StoreSession,
};
use base_exoware_archive_rpc::{
    ArchiveConverter, ArchiveExecutor, ArchiveHandle, ExowareArchiveArgs, ExowareArchiveExtension,
    ExowareHistory, ProviderSource, RETENTION_TTL, RetentionCache,
};
use base_node_runner::test_utils::{
    L1_BLOCK_INFO_DEPOSIT_TX, LocalNodeRetention, RethRpcModule, RpcModuleSelection, TestHarness,
};
use base_protocol::BaseTimeUpdateTx;
use base_test_utils::{
    Account, build_test_genesis, build_test_genesis_cobalt, build_test_genesis_everest,
};
use reth_chainspec::EthChainSpec;
use reth_node_core::args::{PruningArgs, StaticFilesArgs};
use reth_prune_types::PruneSegment;
use reth_storage_api::{
    BlockHashReader, BlockNumReader, BlockReader, PruneCheckpointReader, StateProviderFactory,
    errors::ProviderError,
};
use serde_json::{Value, json};
use tokio::sync::OnceCell;

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

/// Runtime code reading slot 0 back: `mstore(0, sload(0)); return(0, 32)`. Installed over
/// the contract with a state override, which keeps the contract's storage.
const READER: [u8; 11] = [0x60, 0x00, 0x54, 0x60, 0x00, 0x52, 0x60, 0x20, 0x60, 0x00, 0xf3];

/// Init code of a creation call returning `blockhash(number - 1)` and the caller's balance,
/// so a historical call depends on local block hashes and on archived state.
const PROBE: [u8; 19] = [
    0x43, 0x60, 0x01, 0x90, 0x03, 0x40, 0x60, 0x00, 0x52, 0x33, 0x31, 0x60, 0x20, 0x52, 0x60, 0x40,
    0x60, 0x00, 0xf3,
];

/// Init code of a creation call returning `blockhash(number - 1)`.
const HASH_PROBE: [u8; 14] =
    [0x43, 0x60, 0x01, 0x90, 0x03, 0x40, 0x60, 0x00, 0x52, 0x60, 0x20, 0x60, 0x00, 0xf3];

/// `eth_callMany` index inside an active block: after the deposits and the first user
/// transaction (the deployment in block 1, the transfer after), before the contract call.
const MIDDLE_INDEX: u64 = 3;

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
    let mut builder = TestHarness::builder()
        .with_chain_spec(chain_spec)
        .with_retention(retention)
        .with_rpc_modules(RpcModuleSelection::from([
            RethRpcModule::Eth,
            RethRpcModule::Debug,
            RethRpcModule::Trace,
        ]));
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
        out.extend(execution_requests(*number, *hash, contract));
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

/// Execution requests at block `number` (hash `hash`): calls that read archived state and
/// local block hashes, a bundle replayed after part of the block, and traces of the block.
fn execution_requests(number: u64, hash: B256, contract: Address) -> Vec<(&'static str, Value)> {
    let tag = format!("0x{number:x}");
    let alice = Account::Alice.address();
    let probe = json!({ "from": alice, "input": Bytes::from(PROBE) });
    let reader = json!({ contract.to_string(): { "code": Bytes::from(READER) } });
    let mut out = vec![
        ("eth_call", json!([probe, tag])),
        ("eth_call", json!([probe, { "blockHash": hash }])),
        ("eth_call", json!([{ "to": contract }, tag, reader])),
        ("eth_call", json!([{ "to": contract }, format!("0x{:x}", number - 1), reader])),
        ("trace_block", json!([tag])),
        ("debug_traceBlockByNumber", json!([tag, {}])),
        ("debug_traceBlockByNumber", json!([tag, { "tracer": "callTracer" }])),
    ];
    // `blockhash(number - 1)`, then slot 0 of the contract read through the override.
    let bundle = json!([{ "transactions": [
        { "from": alice, "input": Bytes::from(HASH_PROBE) },
        { "from": alice, "to": contract },
    ] }]);
    let at = |index: u64| json!({ "blockNumber": tag, "transactionIndex": index });
    out.push(("eth_callMany", json!([bundle, at(MIDDLE_INDEX), reader])));
    if number <= ACTIVE_BLOCKS {
        out.push(("eth_callMany", json!([bundle, at(tx_count(number)), reader])));
    }
    out
}

/// Transactions in active block `number`: two deposits, the deployment in block 1, then the
/// transfer and the contract call.
const fn tx_count(number: u64) -> u64 {
    if number == 1 { 5 } else { 4 }
}

/// Value active block `number` stores in the contract's slot 0 (0 before block 1).
fn slot_after(number: u64) -> B256 {
    if number == 0 { B256::ZERO } else { B256::from(U256::from(100 + number)) }
}

/// Exact execution answers at active blocks, while the node still holds them (the parity
/// check then holds them after pruning): a historical call sees the parent's hash through
/// `BLOCKHASH` and the caller's balance at the block; the slot is the block's value at N and
/// the parent's at N - 1; a bundle at the middle index sees the parent's slot and one at the
/// transaction count the block's.
async fn check_execution_values(client: &RpcClient, hashes: &[B256], contract: Address) {
    let alice = Account::Alice.address();
    let word = |value: &Value, at: usize| -> B256 {
        let bytes: Bytes = serde_json::from_value(value.clone()).expect("hex output");
        B256::from_slice(&bytes[at * 32..at * 32 + 32])
    };
    for number in 1..=ACTIVE_BLOCKS {
        let requests = execution_requests(number, hashes[number as usize], contract);
        let parent = hashes[number as usize - 1];
        let balance = call(client, "eth_getBalance", json!([alice, format!("0x{number:x}")])).await;
        let balance: U256 = serde_json::from_value(balance).expect("balance");
        for (method, params) in &requests[..2] {
            let value = call(client, method, params.clone()).await;
            assert_eq!(
                word(&value, 0),
                parent,
                "{method} {params}: BLOCKHASH is the parent's hash"
            );
            assert_eq!(
                U256::from_be_bytes(word(&value, 1).0),
                balance,
                "{method} {params}: the caller's balance at block {number}"
            );
        }
        for (request, slot) in [(&requests[2], number), (&requests[3], number - 1)] {
            let value = call(client, request.0, request.1.clone()).await;
            assert_eq!(word(&value, 0), slot_after(slot), "{} {}: slot 0", request.0, request.1);
        }
        let middle_slot = if number == 1 { B256::ZERO } else { slot_after(number - 1) };
        for (request, slot) in [(&requests[7], middle_slot), (&requests[8], slot_after(number))] {
            let value = call(client, request.0, request.1.clone()).await;
            assert_eq!(
                value,
                json!([[{ "value": parent }, { "value": slot }]]),
                "{} {}: the bundle's exact outputs",
                request.0,
                request.1
            );
        }
    }
}

/// Traces of mined transaction `hash`.
fn transaction_traces(hash: B256) -> Vec<(&'static str, Value)> {
    vec![
        ("debug_traceTransaction", json!([hash, {}])),
        ("debug_traceTransaction", json!([hash, { "tracer": "callTracer" }])),
        ("debug_traceTransaction", json!([hash, { "tracer": "prestateTracer" }])),
    ]
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
        out.extend(transaction_traces(*hash));
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

/// Body-only pruning: state, receipts and the transaction lookup stay local, but blocks and
/// the bodies traces replay come from the archive.
#[tokio::test(flavor = "multi_thread")]
async fn body_only_pruning_serves_blocks_from_archive() {
    let retention = LocalNodeRetention {
        pruning: PruningArgs {
            block_interval: Some(1),
            minimum_distance: Some(RETENTION),
            bodies_distance: Some(RETENTION),
            ..Default::default()
        },
        static_files: short_retention().static_files,
    };
    let scenario = Scenario {
        name: "bodies",
        retention,
        pruned: PruneSegment::Bodies,
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

/// An unpruned node with the archive on executes and traces recent blocks successfully with
/// zero store connections.
#[tokio::test(flavor = "multi_thread")]
async fn unpruned_execution_makes_zero_store_reads() {
    tokio::time::timeout(E2E_TIMEOUT, async {
        let (url, connections) = counting_store().await;
        let chain_spec = Arc::new(BaseChainSpec::from_genesis(build_test_genesis()));
        let harness = archive_node(
            chain_spec,
            LocalNodeRetention::default(),
            archive_args(&url, false),
            None,
        )
        .await;
        let client = harness.rpc_client().expect("rpc client");
        let (contract, tx_hashes) = build_active_blocks(&harness).await;
        let provider = harness.blockchain_provider();
        let mut requests = Vec::new();
        for number in 1..=ACTIVE_BLOCKS {
            let block = provider.block_by_number(number).expect("read").expect("block exists");
            let hash = alloy_primitives::Sealable::hash_slow(&block.header);
            requests.extend(execution_requests(number, hash, contract));
        }
        for hash in &tx_hashes {
            requests.extend(transaction_traces(*hash));
        }
        for (method, params) in &requests {
            let value = call(&client, method, params.clone()).await;
            assert!(
                value.get("code").is_none() && value.get("transport").is_none(),
                "{method} {params} succeeds on an unpruned node, got {value}"
            );
        }
        assert_eq!(
            connections.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "{} recent executions and traces make zero store connections",
            requests.len()
        );
    })
    .await
    .expect("test timed out");
}

/// An execution paused across an archive prune is refused at its conclusion (4444), also
/// when the prune deleted the posting a later read needed and the reader fell back to the
/// node's state at the archive's head.
///
/// This drives `ExowareHistory` directly: pausing an RPC execution between two of its store
/// reads would need a per-request interposer on the store, but the node's reads and the
/// exporter's writes share one URL and one multiplexed HTTP/2 connection, so a proxy cannot
/// hold one without stalling the prune it waits for. `archived_task`, which every RPC
/// execution over archived state goes through, turns the failed conclusion into 4444 (see
/// its unit tests).
#[tokio::test(flavor = "multi_thread")]
async fn execution_paused_across_an_archive_prune_is_refused() {
    tokio::time::timeout(E2E_TIMEOUT, async {
        let (simulator, url) = exoware_simulator::open_temp().await.expect("simulator starts");
        let store = Arc::new(
            ExowareStore::connect(&url, NAMESPACE, Duration::from_secs(5)).expect("store"),
        );
        let chain_spec = Arc::new(BaseChainSpec::from_genesis(build_test_genesis()));
        let mut args = archive_args(&url, true);
        // Segment 0 stays served until the archive's head reaches segment `RETAIN`; the node
        // prunes block 2 well before that, even when its pruner lags the head.
        const RETAIN: u64 = 16;
        args.archive_exoware_retain_segments = Some(RETAIN);
        args.archive_exoware_prune_grace_segments = 1;
        let harness =
            archive_node(Arc::clone(&chain_spec), short_retention(), args.clone(), None).await;
        build_active_blocks(&harness).await;
        let provider = harness.blockchain_provider();
        let number = 2;
        while provider
            .get_prune_checkpoint(PruneSegment::AccountHistory)
            .expect("checkpoint read")
            .and_then(|c| c.block_number)
            .is_none_or(|pruned| pruned < number)
        {
            harness.advance_chain(BLOCKS_PER_SEGMENT).await.expect("advance");
        }
        let head = harness.latest_block().number;
        wait_for_segment(&store, head / BLOCKS_PER_SEGMENT - 2).await;
        let highest = ExowareArchive::<ExowareStore>::highest_marker(&store.session())
            .await
            .expect("marker read")
            .expect("published")
            .segment;
        assert!(
            highest < RETAIN,
            "segment 0 is still served when the execution starts ({highest})"
        );

        let hash = provider.block_hash(number).expect("read").expect("canonical hash");
        let history = ExowareHistory {
            archive: Arc::new(ArchiveHandle {
                store: Arc::clone(&store),
                node: Arc::new(ProviderSource::new(provider.clone())),
                verifier: BlockVerifier::new(Arc::clone(&chain_spec)),
                chain_id: chain_spec.chain().id(),
                genesis_hash: chain_spec.genesis_hash(),
                limits: ReaderLimits::default(),
                budget: Duration::from_secs(5),
                retry_after: RETENTION_TTL,
                reader: OnceCell::new(),
                open_failed_at: Default::default(),
            }),
            provider: provider.clone(),
            converter: ArchiveConverter::new(Arc::clone(&chain_spec)),
            retention: RetentionCache::new(RETENTION_TTL),
            executor: Arc::new(ArchiveExecutor::new().expect("runtime")),
            execution_budget: Duration::from_secs(120),
        };
        let PrunedState { state, conclude, deadline } = history
            .state_at(number, hash, std::time::Instant::now())
            .await
            .expect("archived state opens");
        assert!(
            deadline > std::time::Instant::now() + Duration::from_secs(100),
            "the execution budget bounds the whole execution"
        );
        let state = Arc::new(std::sync::Mutex::new(state));
        let read = |address: Address| {
            let state = Arc::clone(&state);
            tokio::task::spawn_blocking(move || {
                state.lock().expect("state").basic_account(&address)
            })
        };
        let alice = read(Account::Alice.address()).await.expect("read task");
        assert!(
            alice.as_ref().is_ok_and(|account| account.is_some_and(|a| a.nonce == 4)),
            "Alice sent four transactions by the end of block 2, got {alice:?}"
        );

        // The archive moves past segment 0 and physically deletes its rows.
        let bob_posting = ArchiveKey::state(&StateKey::account(Account::Bob.address()), 0);
        let mut deleted = false;
        for _ in 0..2 * RETAIN {
            harness.advance_chain(BLOCKS_PER_SEGMENT).await.expect("advance");
            let head = harness.latest_block().number;
            wait_for_segment(&store, head / BLOCKS_PER_SEGMENT - 2).await;
            if store.session().get(&bob_posting).await.expect("posting read").is_none() {
                deleted = true;
                break;
            }
        }
        assert!(deleted, "the archive physically pruned segment 0");
        // With its posting gone the read either finds no change and falls back to the node at
        // the request's archive head (the silent miss) or, once the node pruned that head too,
        // fails. Neither may become a success.
        let bob = read(Account::Bob.address()).await.expect("read task");
        assert!(
            matches!(bob, Ok(_) | Err(ProviderError::StateAtBlockPruned(2))),
            "the late read is a miss or a typed refusal, got {bob:?}"
        );
        let err = conclude.await.expect_err("the execution is refused at its conclusion");
        // The marker re-check wins over the read's own outcome; the earliest block moves
        // with the exporter, so only the prefix is fixed.
        assert!(
            err.to_string().starts_with(
                "pruned history unavailable: block 2 is pruned from the archive (earliest archived block "
            ),
            "the refusal names the prune, got {err}"
        );
        simulator.abort();
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
    // The node keeps every canonical header, so a block hash it does not know is never pruned
    // history: the archive's block-hash locator is not consulted and the node's answer stands
    // (`archive_requests.rs` counts zero store requests for it).
    for (method, params, expected) in [
        ("eth_getBlockByHash", json!([forged_block, true]), Value::Null),
        ("eth_getBlockTransactionCountByHash", json!([forged_block]), Value::Null),
        (
            "eth_getLogs",
            json!([{ "blockHash": forged_block }]),
            json!({ "code": -32001, "message": format!("block not found: hash {forged_block}") }),
        ),
    ] {
        let value = call(client, method, params.clone()).await;
        assert_eq!(
            value, expected,
            "{name}: {method} {params} with a forged locator keeps the node's answer"
        );
    }
    let trace = call(client, "debug_traceTransaction", json!([forged_tx, {}])).await;
    if lookup_pruned {
        assert_eq!(
            trace["code"],
            json!(PRUNED_HISTORY),
            "{name}: tracing a transaction with a forged locator is 4444, got {trace}"
        );
    } else {
        assert_eq!(
            trace,
            json!({ "code": -32000, "message": "transaction not found" }),
            "{name}: tracing an unknown transaction keeps reth's error while the lookup is unpruned"
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
    let genesis = provider.block_hash(0).expect("read").expect("genesis hash");
    let hashes: Vec<B256> =
        std::iter::once(genesis).chain(blocks.iter().map(|(_, hash)| *hash)).collect();
    check_execution_values(&client, &hashes, contract).await;
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
        // reth opens pruned state and refuses it on its first read, which is the refusal the
        // execution path probes for before running anything.
        let opened = provider.history_by_block_number(1);
        assert!(opened.is_ok(), "pruned state opens and refuses only when read");
        let pruned_state = opened.and_then(|state| state.basic_account(&Account::Alice.address()));
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

    check_failure_mid_execution(&client, &store, contract, &tx_hashes).await;
    check_failure_in_trace_output(&client, &store, tx_hashes[5]).await;

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
        ("eth_call", execution_requests(2, blocks[1].1, contract)[0].1.clone()),
        ("eth_callMany", execution_requests(2, blocks[1].1, contract)[7].1.clone()),
        ("trace_block", json!(["0x2"])),
        ("debug_traceBlockByNumber", json!(["0x2", {}])),
        ("debug_traceTransaction", json!([tx_hashes[1], {}])),
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

/// A store row that fails verification only when the EVM reads it: the contract's slot-0
/// posting in segment 0 is overwritten after export. Executions over archived state that
/// read the slot (a call, a bundle, block traces and a prestate trace, which would otherwise
/// swallow the read error) are 4444 with a reason, never a result.
async fn check_failure_mid_execution(
    client: &RpcClient,
    store: &ExowareStore,
    contract: Address,
    tx_hashes: &[B256],
) {
    store
        .put(&[(ArchiveKey::state(&StateKey::storage(contract, B256::ZERO), 0), vec![0xff].into())])
        .await
        .expect("corrupt the posting");
    let at = execution_requests(3, B256::ZERO, contract);
    for (method, params) in [
        ("eth_call", at[2].1.clone()),
        ("eth_callMany", at[7].1.clone()),
        ("trace_block", json!(["0x3"])),
        ("debug_traceBlockByNumber", json!(["0x3", { "tracer": "callTracer" }])),
        ("debug_traceTransaction", json!([tx_hashes[6], { "tracer": "prestateTracer" }])),
    ] {
        let value = call(client, method, params.clone()).await;
        assert_eq!(
            value["code"],
            json!(PRUNED_HISTORY),
            "{method} {params} over a corrupt posting is 4444, got {value}"
        );
        assert!(
            value["message"].as_str().is_some_and(|m| m.starts_with("pruned history unavailable")),
            "{method} {params} names pruned history, got {value}"
        );
    }
}

/// A read failure only trace output reaches: block 3's archived changeset gives a fee
/// recipient a code hash the node has no bytecode for. Executing the block's transfer only
/// credits that account, so its default trace is unchanged; the prestate tracer then loads
/// the code for its output and swallows the error (`code_by_hash_ref(..).ok()`), yet the
/// answer is exactly 4444, never a prestate without the code.
async fn check_failure_in_trace_output(client: &RpcClient, store: &ExowareStore, transfer: B256) {
    let default = call(client, "debug_traceTransaction", json!([transfer, {}])).await;
    assert!(default.get("code").is_none(), "the transfer traces before the change, got {default}");
    let key = ArchiveKey::block(Family::AccountChanges, 3);
    let raw = store.session().get(&key).await.expect("changeset read").expect("block 3 row");
    let mut changes = BlockCodec::decode_account_changes(&raw).expect("changeset decodes");
    let recipient = changes
        .iter_mut()
        .find(|change| {
            change.address != Account::Alice.address()
                && change.address != Account::Bob.address()
                && change.before.is_some_and(|before| before.balance > U256::ZERO)
        })
        .expect("block 3 credits a funded fee recipient");
    let address = recipient.address;
    if let Some(before) = recipient.before.as_mut() {
        before.code_hash = B256::repeat_byte(0xcd);
    }
    let row = BlockCodec::encode_account_changes(3, &changes).expect("changeset encodes");
    store.put(&[(key, row.into())]).await.expect("rewrite the changeset");

    let after = call(client, "debug_traceTransaction", json!([transfer, {}])).await;
    assert_eq!(after, default, "executing the transfer never reads {address}'s code");
    let prestate =
        call(client, "debug_traceTransaction", json!([transfer, { "tracer": "prestateTracer" }]))
            .await;
    assert_eq!(
        prestate,
        json!({
            "code": PRUNED_HISTORY,
            "message": format!(
                "pruned history unavailable: local read failed: bytecode {} is missing on the node",
                B256::repeat_byte(0xcd)
            ),
        }),
        "a code read only the prestate output makes is exactly 4444"
    );
}

/// On an unpruned node whose store is unreachable, misses (an unmined receipt, an unknown
/// hash, a block above the head) and recent reads answer locally. An exporter whose shutdown
/// is already signaled does not provision, and one waiting for a config row keeps retrying.
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

        // A non-provisioning exporter that starts before the config row exists (a rollout
        // race) keeps retrying instead of halting, and still exits cleanly on shutdown.
        let mut waiting = archive_args(&url, true);
        waiting.archive_exoware_provision = false;
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let exporter = tokio::spawn(ExowareArchiveExtension::export(
            Arc::clone(&store),
            harness.blockchain_provider(),
            BlockVerifier::new(Arc::clone(&chain_spec)),
            waiting,
            chain_spec.chain().id(),
            chain_spec.genesis_hash(),
            async move {
                let _ = stopped.await;
            },
        ));
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(!exporter.is_finished(), "a missing config row is retried, not terminal");
        stop.send(()).expect("exporter is still waiting for shutdown");
        assert_eq!(
            exporter.await.expect("exporter task joins"),
            Ok(()),
            "a retrying exporter exits cleanly on shutdown"
        );
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
