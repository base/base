//! Counts archive requests per RPC on a pruned node. This is its own test binary because the
//! count is the process-wide `reth_base_exoware_archive_requests` counter, which another node in the
//! same process would also move.

use std::{sync::Arc, time::Duration};

use alloy_consensus::Sealed;
use alloy_eips::{Decodable2718, Encodable2718};
use alloy_primitives::{B256, Bytes, U256};
use alloy_rpc_client::RpcClient;
use base_common_consensus::BaseTxEnvelope;
use base_execution_chainspec::BaseChainSpec;
use base_exoware_archive::{ArchiveStore, ExowareArchive, ExowareStore};
use base_exoware_archive_rpc::{ExowareArchiveArgs, ExowareArchiveExtension};
use base_node_runner::test_utils::{
    L1_BLOCK_INFO_DEPOSIT_TX, LocalNodeRetention, RethRpcModule, RpcModuleSelection, TestHarness,
};
use base_protocol::BaseTimeUpdateTx;
use base_test_utils::build_test_genesis;
use reth_node_core::args::{PruningArgs, StaticFilesArgs};
use reth_storage_api::{BlockReader, errors::ProviderError};
use serde_json::{Value, json};

const BLOCKS_PER_SEGMENT: u64 = 8;
const RETENTION: u64 = 64;
const NAMESPACE: &str = "e2e-requests/";
const E2E_TIMEOUT: Duration = Duration::from_secs(300);
const EXPORT_TIMEOUT: Duration = Duration::from_secs(60);
/// Blocks the node's RPC block cache holds, far fewer than [`RETENTION`], so a pruned block
/// is never answered from the cache, as on a long-running node.
const RPC_CACHE_BLOCKS: u32 = 4;
/// The rendered name of the archive request counter (the node prefixes metrics with `reth_`).
const REQUESTS: &str = "reth_base_exoware_archive_requests";

/// The process-wide count of archive requests begun, from the node's Prometheus recorder.
fn archive_requests() -> u64 {
    let rendered = reth_node_metrics::recorder::install_prometheus_recorder().handle().render();
    rendered
        .lines()
        .find_map(|line| line.strip_prefix(REQUESTS)?.strip_prefix(' '))
        .map_or(0, |value| value.trim().parse().expect("counter value"))
}

/// The harness's L1 info deposit with a per-block source hash.
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

/// A node that keeps only the last [`RETENTION`] blocks of bodies, receipts and state.
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

/// A TCP proxy to the store at `upstream` that can be cut: once the returned flag is set it
/// closes every open connection and refuses new ones, so the store is down even for a client
/// holding a live connection. Returns the proxy URL, the flag, and the proxy task, which ends
/// once every connection is closed.
async fn severable_store(
    upstream: &str,
) -> (String, Arc<tokio::sync::Notify>, tokio::task::JoinHandle<()>) {
    let upstream = upstream.trim_start_matches("http://").to_string();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let url = format!("http://{}", listener.local_addr().expect("addr"));
    let cut = Arc::new(tokio::sync::Notify::new());
    let severed = Arc::clone(&cut);
    let proxy = tokio::spawn(async move {
        let mut connections = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                () = severed.notified() => break,
                accepted = listener.accept() => {
                    let Ok((mut inbound, _)) = accepted else { break };
                    let upstream = upstream.clone();
                    connections.spawn(async move {
                        let Ok(mut outbound) = tokio::net::TcpStream::connect(upstream).await
                        else {
                            return;
                        };
                        let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
                    });
                }
            }
        }
        // Dropping the listener refuses new connections; aborting the tasks closes open ones.
        drop(listener);
        connections.abort_all();
        while connections.join_next().await.is_some() {}
    });
    (url, cut, proxy)
}

/// Calls `eth_getBlockByNumber(number, full)` and returns its result or error as JSON, with
/// the archive requests it began.
async fn counted_block(client: &RpcClient, number: u64, full: bool) -> (Value, u64) {
    let before = archive_requests();
    let params = json!([format!("0x{number:x}"), full]);
    let value = match client.request::<_, Value>("eth_getBlockByNumber", params).await {
        Ok(value) => value,
        Err(err) => err.as_error_resp().map_or_else(
            || panic!("transport failure: {err}"),
            |resp| json!({ "code": resp.code, "message": resp.message }),
        ),
    };
    (value, archive_requests() - before)
}

/// A block read over pruned bodies reaches the archive exactly once: with every deposit field
/// served from the archive (`full = true` over pruned receipts), and as one 4444 with the
/// store down. The node's local delegate never asks the archive a second time.
#[tokio::test(flavor = "multi_thread")]
async fn block_reads_over_pruned_bodies_make_one_archive_request() {
    tokio::time::timeout(E2E_TIMEOUT, async {
        let (simulator, direct) = exoware_simulator::open_temp().await.expect("simulator starts");
        let store =
            ExowareStore::connect(&direct, NAMESPACE, Duration::from_secs(5)).expect("store");
        let (url, cut, proxy) = severable_store(&direct).await;
        let chain_spec = Arc::new(BaseChainSpec::from_genesis(build_test_genesis()));
        let args = ExowareArchiveArgs {
            archive_exoware_url: Some(url.clone()),
            archive_exoware_namespace: NAMESPACE.to_string(),
            archive_exoware_export: true,
            archive_exoware_provision: true,
            archive_exoware_segment_blocks: BLOCKS_PER_SEGMENT,
            archive_exoware_timeout: Duration::from_secs(2),
            ..ExowareArchiveArgs::default()
        };
        let harness = TestHarness::builder()
            .with_chain_spec(chain_spec)
            .with_retention(short_retention())
            .with_rpc_modules(RpcModuleSelection::from([RethRpcModule::Eth]))
            .with_rpc_cache_blocks(RPC_CACHE_BLOCKS)
            .with_ext::<ExowareArchiveExtension>(args)
            .build()
            .await
            .expect("node starts");
        let client = harness.rpc_client().expect("rpc client");
        for number in 1..=2 {
            harness
                .build_block_from_transactions(vec![
                    l1_info_deposit(number),
                    base_time_deposit(number),
                ])
                .await
                .expect("deposit block");
        }
        let (unpruned, requests) = counted_block(&client, 1, true).await;
        assert_eq!(requests, 0, "a retained block makes no archive request");
        assert_eq!(
            unpruned["transactions"].as_array().map(Vec::len),
            Some(2),
            "block 1 holds both deposits: {unpruned}"
        );

        let provider = harness.blockchain_provider();
        while !matches!(provider.block_by_number(2), Err(ProviderError::BlockExpired { .. })) {
            harness.advance_chain(BLOCKS_PER_SEGMENT).await.expect("advance");
        }
        let head = harness.latest_block().number;
        wait_for_segment(&store, head / BLOCKS_PER_SEGMENT - 2).await;

        let (archived, requests) = counted_block(&client, 1, true).await;
        assert_eq!(archived, unpruned, "the archive serves block 1 with every deposit field");
        assert_eq!(requests, 1, "full = true over pruned bodies makes one archive request");

        cut.notify_one();
        proxy.await.expect("proxy closes");
        let (down, requests) = counted_block(&client, 2, false).await;
        assert_eq!(down["code"], json!(4444), "store down over pruned bodies is 4444: {down}");
        assert_eq!(requests, 1, "store down makes one archive request");
        simulator.abort();
    })
    .await
    .expect("e2e run timed out");
}
