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
use base_exoware_archive::{ArchiveStore, ExowareArchive, ExowareStore};
use base_exoware_archive_rpc::{ExowareArchiveArgs, ExowareArchiveExtension};
use base_node_runner::test_utils::{L1_BLOCK_INFO_DEPOSIT_TX, LocalNodeRetention, TestHarness};
use base_protocol::BaseTimeUpdateTx;
use base_test_utils::{Account, build_test_genesis, build_test_genesis_cobalt, build_test_genesis_everest};
use reth_node_core::args::{PruningArgs, StaticFilesArgs};
use reth_storage_api::{BlockNumReader, BlockReader, StateProviderFactory, errors::ProviderError};
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
) -> TestHarness {
    TestHarness::builder()
        .with_chain_spec(chain_spec)
        .with_retention(retention)
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
        Err(err) => match err.as_error_resp() {
            Some(payload) => json!({ "code": payload.code, "message": payload.message }),
            None => json!({ "transport": err.to_string(), "method": method, "params": params }),
        },
    }
}

/// Number-addressed requests over the active blocks.
fn number_requests(blocks: &[(u64, B256)], contract: Address) -> Vec<(&'static str, Value)> {
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
    }
    let last = blocks.last().expect("blocks").0;
    out.push(("eth_getLogs", json!([{ "fromBlock": "0x1", "toBlock": format!("0x{last:x}") }])));
    out.push((
        "eth_getLogs",
        json!([{ "fromBlock": "0x1", "toBlock": "latest", "address": contract }]),
    ));
    out
}

/// Hash-addressed requests over the active blocks and transactions.
fn hash_requests(blocks: &[(u64, B256)], tx_hashes: &[B256]) -> Vec<(&'static str, Value)> {
    let mut out = Vec::new();
    for (number, hash) in blocks {
        out.push(("eth_getBlockByHash", json!([hash, true])));
        out.push(("eth_getBlockTransactionCountByHash", json!([hash])));
        if *number <= ACTIVE_BLOCKS {
            out.push(("eth_getTransactionByBlockHashAndIndex", json!([hash, "0x2"])));
            out.push(("eth_getBlockReceipts", json!([{ "blockHash": hash }])));
        }
    }
    for hash in tx_hashes {
        out.push(("eth_getTransactionByHash", json!([hash])));
        out.push(("eth_getTransactionReceipt", json!([hash])));
    }
    out.push(("eth_getLogs", json!([{ "blockHash": blocks[2].1 }])));
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

/// A pruned node serves every covered `eth_` method from the archive with JSON parity; with
/// the store down, number-addressed reads in the pruned range are 4444 and an unknown hash is
/// `null`.
#[tokio::test(flavor = "multi_thread")]
async fn pruned_node_serves_history_from_archive() {
    tokio::time::timeout(E2E_TIMEOUT, pruned_node_parity()).await.expect("e2e run timed out");
}

/// Body of [`pruned_node_serves_history_from_archive`].
async fn pruned_node_parity() {
    let (simulator, url) = exoware_simulator::open_temp().await.expect("simulator starts");
    let store = ExowareStore::connect(&url, NAMESPACE, Duration::from_secs(5)).expect("store");
    let chain_spec = Arc::new(BaseChainSpec::from_genesis(build_test_genesis()));
    let harness = archive_node(chain_spec, short_retention(), archive_args(&url, true)).await;
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
    let number_requests = number_requests(&blocks, contract);
    let hash_requests = hash_requests(&blocks, &tx_hashes);
    let requests: Vec<_> = number_requests.iter().chain(&hash_requests).cloned().collect();
    let mut expected = Vec::with_capacity(requests.len());
    for (method, params) in &requests {
        expected.push(call(&client, method, params.clone()).await);
    }
    for ((method, params), value) in requests.iter().zip(&expected) {
        assert!(
            value.get("code").is_none() && value.get("transport").is_none() && !value.is_null(),
            "{method} {params} must succeed before pruning, got {value}"
        );
    }

    // Export while the chain grows past the retention window: the finalized head trails the
    // head by one block, so segment k is published once block 8k+8 exists.
    wait_for_segment(&store, 0).await;
    while harness.latest_block().number < TIP {
        harness.advance_chain(BLOCKS_PER_SEGMENT).await.expect("advance");
        let head = harness.latest_block().number;
        wait_for_segment(&store, head / BLOCKS_PER_SEGMENT - 2).await;
    }

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
    let earliest = provider.earliest_block_number().expect("earliest block");
    assert!(earliest > ACTIVE_BLOCKS + 2, "every compared block is pruned (earliest {earliest})");

    for ((method, params), before) in requests.iter().zip(&expected) {
        let after = call(&client, method, params.clone()).await;
        assert_eq!(&after, before, "{method} {params} must match the pre-pruning response");
    }

    // A block above the head stays a local `null` on a pruned node.
    let head = harness.latest_block().number;
    let next =
        call(&client, "eth_getBlockByNumber", json!([format!("0x{:x}", head + 1), false])).await;
    assert_eq!(next, Value::Null, "block {} does not exist yet", head + 1);

    // Store down: number-addressed reads in the pruned range are 4444 with a reason, an
    // unknown hash keeps the node's `null`, and recent blocks are still served locally.
    simulator.abort();
    let _ = simulator.await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    for (method, params) in [
        ("eth_getBlockByNumber", json!(["0x2", false])),
        ("eth_getBalance", json!([Account::Alice.address(), "0x2"])),
        ("eth_getStorageAt", json!([contract, "0x0", "0x2"])),
        ("eth_getLogs", json!([{ "fromBlock": "0x1", "toBlock": "0x3" }])),
        ("eth_getBlockByHash", json!([blocks[1].1, false])),
    ] {
        let value = call(&client, method, params.clone()).await;
        assert_eq!(value["code"], json!(PRUNED_HISTORY), "{method} {params} is 4444 with the store down, got {value}");
        assert!(
            value["message"].as_str().is_some_and(|m| m.starts_with("pruned history unavailable")),
            "{method} {params} names pruned history, got {value}"
        );
    }
    let unknown = B256::repeat_byte(0x42);
    for (method, params) in [
        ("eth_getTransactionReceipt", json!([unknown])),
        ("eth_getTransactionByHash", json!([unknown])),
        ("eth_getBlockByHash", json!([unknown, false])),
    ] {
        let value = call(&client, method, params.clone()).await;
        assert_eq!(value, Value::Null, "{method} of an unknown hash stays null with the store down");
    }
    let latest = call(&client, "eth_getBlockByNumber", json!(["latest", false])).await;
    assert_eq!(
        latest["number"],
        json!(format!("0x{head:x}")),
        "recent blocks are served locally with the store down"
    );
}

/// On an unpruned node whose store is unreachable, misses (an unmined receipt, an unknown
/// hash, a block above the head) and recent reads answer locally.
#[tokio::test(flavor = "multi_thread")]
async fn unpruned_node_with_store_down_answers_locally() {
    tokio::time::timeout(E2E_TIMEOUT, async {
        let chain_spec = Arc::new(BaseChainSpec::from_genesis(build_test_genesis()));
        let harness =
            archive_node(chain_spec, LocalNodeRetention::default(), archive_args("http://127.0.0.1:1", false))
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
        let genesis = call(&client, "eth_getBlockByNumber", json!(["0x0", false])).await;
        assert!(genesis["hash"].is_string(), "genesis is served locally, got {genesis}");
        let balance = call(&client, "eth_getBalance", json!([Account::Alice.address(), "0x1"])).await;
        assert!(balance.is_string(), "unpruned state is served locally, got {balance}");
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
            let with_archive = call(&archive_client, "eth_getTransactionCount", params.clone()).await;
            let base_tx = call(&plain_client, "eth_getTransactionCount", params.clone()).await;
            assert_eq!(with_archive, base_tx, "{label}: archive must delegate to the Base override");
            if label == "everest" {
                assert_eq!(with_archive, json!("0x2a"), "everest: seeded channel nonce 42");
            } else {
                assert_eq!(with_archive["code"], json!(-32602), "pre-everest: invalid params, got {with_archive}");
            }
            let legacy = call(&archive_client, "eth_getTransactionCount", json!([alice, "latest"])).await;
            let zero = call(&archive_client, "eth_getTransactionCount", json!([alice, "latest", "0x0"])).await;
            let plain_legacy = call(&plain_client, "eth_getTransactionCount", json!([alice, "latest"])).await;
            assert_eq!(legacy, zero, "{label}: nonce_key 0 equals the protocol nonce");
            assert_eq!(legacy, plain_legacy, "{label}: protocol nonce matches the plain node");
        }
    })
    .await
    .expect("test timed out");
}
