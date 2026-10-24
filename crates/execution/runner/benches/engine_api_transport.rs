//! Engine API transport benchmark: the auth IPC socket versus the in-memory call.
//!
//! These are the numbers behind "Up to 5× faster Engine API"
//! (<https://bdocs.cbhq.net/d/up-to-5x-faster-engine-api>). One unified Base node is launched
//! with its auth IPC endpoint enabled, and the same Engine API calls are driven through both
//! consensus client endpoints:
//!
//! - `ipc`: [`EngineEndpoint::Url`] with a `file://` URL, so JSON-RPC over the auth IPC socket.
//! - `in-memory`: [`EngineEndpoint::InMemory`], direct calls into the typed Engine API handler.
//!
//! Each iteration builds on genesis with a fresh timestamp, so transaction nonces never advance:
//!
//!  1. `engine_forkchoiceUpdatedV3` with `attributes.transactions` (timed for the syncing metric).
//!  2. Wait for the payload to build (untimed).
//!  3. `engine_getPayloadV4`, then `engine_newPayloadV4` (timed for both metrics).
//!
//! The sequencer metric (fetch and import) is step 3. The syncing-node metric (build, fetch, and
//! import) is steps 1 and 3. `engine_newPayloadV4` resubmits a block the node built itself, so
//! neither metric includes block execution, and both paths reach the same handler: the gap
//! between them is transport cost.
//!
//! Synthetic workloads are transaction mixes signed by a test account. Set
//! `ENGINE_BENCH_MAINNET_RPC` to also replay real Base mainnet blocks. Their transactions keep
//! their calldata and recipient but are re-signed by the test account (contract creations become
//! calls), so payload bytes track mainnet while execution does not.
//!
//! Run with `just fast-engine-api-bench`, or:
//!
//! ```bash
//! RUST_LOG=error cargo bench -p base-node-runner --bench engine_api_transport
//! ```
//!
//! Environment:
//!
//! - `ENGINE_BENCH_ITERS`: timed iterations per path and workload (default 10).
//! - `ENGINE_BENCH_MAINNET_RPC`: a Base mainnet RPC URL, such as `https://mainnet.base.org`.
//! - `ENGINE_BENCH_MAINNET_BLOCKS`: comma-separated blocks to replay. Join numbers with `+` to
//!   stitch several blocks into one payload (default `52915364,52915363,52915358`).

use std::{
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

use alloy_consensus::{SignableTransaction, Transaction};
use alloy_eips::{BlockNumberOrTag, Typed2718, eip2718::Encodable2718};
use alloy_primitives::{Address, B64, B256, Bytes, TxKind, U256};
use alloy_provider::{Provider, RootProvider};
use alloy_rpc_client::RpcClient;
use alloy_rpc_types::Header;
use alloy_rpc_types_engine::{ForkchoiceState, JwtSecret, PayloadAttributes};
use alloy_signer::SignerSync;
use base_common_consensus::{DEPOSIT_TX_TYPE_ID, JovianExtraData};
use base_common_genesis::RollupConfig;
use base_common_network::{Base, BaseEngineApi};
use base_common_rpc_types::BaseTransactionRequest;
use base_common_rpc_types_engine::BasePayloadAttributes;
use base_consensus_engine::{
    BaseEngineClient, EngineClient, EngineClientBuilder, EngineEndpoint, InMemoryTransport,
};
use base_consensus_providers::L1_RPC_TIMEOUT;
use base_execution_chainspec::BaseChainSpec;
use base_node_core::args::RollupArgs;
use base_node_runner::{
    BaseNodeRunner,
    test_utils::{BLOCK_TIME_SECONDS, L1_BLOCK_INFO_DEPOSIT_TX, init_silenced_tracing},
};
use base_test_utils::{Account, DEVNET_CHAIN_ID, build_test_genesis};
use eyre::{Result, WrapErr, eyre};
use reth_db::{ClientVersion, init_db, mdbx::DatabaseArguments, test_utils::tempdir_path};
use reth_node_builder::{NodeBuilder, NodeConfig};
use reth_node_core::{
    args::{DatadirArgs, DiscoveryArgs, NetworkArgs, RpcServerArgs},
    dirs::{DataDirPath, MaybePlatformPath},
};
use reth_tasks::Runtime;
use url::Url;

/// Block gas limit, matching Base mainnet so the largest synthetic mixes fit in one block.
const BLOCK_GAS_LIMIT: u64 = 400_000_000;

/// Upper bound on calldata gas per byte, from the EIP-7623 floor for non-zero bytes.
const MAX_CALLDATA_GAS_PER_BYTE: u64 = 40;

/// Gas for a plain call with no calldata.
const BASE_TX_GAS: u64 = 21_000;

/// Fee cap for every signed transaction, at or above the genesis base fee.
const MAX_FEE_PER_GAS_WEI: u128 = 1_000_000_000;

/// Untimed wait for a payload to build, plus extra time per transaction byte.
const BUILD_WAIT_BASE: Duration = Duration::from_millis(200);
const BUILD_WAIT_PER_MIB: Duration = Duration::from_millis(300);

/// Database map size for the benchmark node, instead of reth's 8 TB default.
const DB_MAX_SIZE_BYTES: usize = 100 * 1024 * 1024;

const DEFAULT_ITERS: usize = 10;
const DEFAULT_MAINNET_BLOCKS: &str = "52915364,52915363,52915358";

/// Synthetic mixes: name, transaction count, and calldata bytes per transaction.
const SYNTHETIC: [(&str, u64, usize); 6] = [
    ("1k transfers", 1_000, 0),
    ("3k transfers", 3_000, 0),
    ("1k calls, 512 B calldata", 1_000, 512),
    ("2k calls, 1 KB calldata", 2_000, 1_024),
    ("2k calls, 2 KB calldata", 2_000, 2_048),
    ("1k calls, 4 KB calldata", 1_000, 4_096),
];

/// One payload's worth of transactions to push through both paths.
struct Workload {
    name: String,
    transactions: Vec<Bytes>,
}

/// Mean round trips for one path and workload.
#[derive(Default)]
struct Timings {
    sequencer: Duration,
    syncing: Duration,
}

fn main() -> Result<()> {
    init_silenced_tracing();
    let iters = std::env::var("ENGINE_BENCH_ITERS")
        .ok()
        .map(|v| v.parse::<usize>())
        .transpose()
        .wrap_err("ENGINE_BENCH_ITERS must be a positive integer")?
        .unwrap_or(DEFAULT_ITERS)
        .max(1);
    tokio::runtime::Builder::new_multi_thread().enable_all().build()?.block_on(run(iters))
}

async fn run(iters: usize) -> Result<()> {
    let mut workloads: Vec<Workload> = SYNTHETIC
        .iter()
        .map(|&(name, count, calldata)| {
            Ok(Workload {
                name: name.to_owned(),
                transactions: (0..count)
                    .map(|nonce| {
                        sign(nonce, Account::Bob.address(), synthetic_calldata(nonce, calldata))
                    })
                    .collect::<Result<_>>()?,
            })
        })
        .collect::<Result<_>>()?;
    if let Some(rpc) = std::env::var("ENGINE_BENCH_MAINNET_RPC").ok().filter(|v| !v.is_empty()) {
        let blocks = std::env::var("ENGINE_BENCH_MAINNET_BLOCKS")
            .unwrap_or_else(|_| DEFAULT_MAINNET_BLOCKS.to_owned());
        workloads.extend(mainnet_workloads(&rpc, &blocks).await?);
    }

    let data_path = tempdir_path();
    let ipc_path =
        std::env::temp_dir().join(format!("engine_api_bench_{}.ipc", std::process::id()));
    let (runtime, launched) = launch_node(&data_path, &ipc_path).await?;
    let in_memory = engine_client(EngineEndpoint::InMemory {
        transport: InMemoryTransport { module: launched.auth_module.clone() },
        engine_api: launched.engine_api.clone(),
    })
    .await?;
    let ipc_url =
        Url::from_file_path(&ipc_path).map_err(|()| eyre!("IPC path must be absolute"))?;
    let ipc = connect_ipc(ipc_url).await?;

    let genesis = in_memory
        .l2_block_by_label(BlockNumberOrTag::Number(0))
        .await?
        .ok_or_else(|| eyre!("missing genesis block"))?
        .header;
    let base_fee_params =
        BaseChainSpec::from_genesis(build_test_genesis()).base_fee_params_at_timestamp(0);
    let eip_1559_params = B64::from(
        ((base_fee_params.max_change_denominator as u64) << 32)
            | base_fee_params.elasticity_multiplier as u64,
    );
    let mut timestamp = genesis.timestamp;
    let mut next_attributes = |transactions: &[Bytes]| {
        timestamp += BLOCK_TIME_SECONDS;
        attributes(&genesis, timestamp, transactions, eip_1559_params)
    };
    let genesis_state = ForkchoiceState {
        head_block_hash: genesis.hash,
        safe_block_hash: genesis.hash,
        finalized_block_hash: genesis.hash,
    };

    println!(
        "Engine API round trips, means of {iters} iterations. Sequencer: getPayload + newPayload. \
         Syncing node: forkchoiceUpdated with transactions + getPayload + newPayload.\n"
    );
    println!(
        "| Workload | Txs | KB | Sequencer IPC | Sequencer in-memory | Speedup | Syncing IPC | \
         Syncing in-memory | Speedup |"
    );
    println!("|---|---:|---:|---:|---:|---:|---:|---:|---:|");
    for workload in &workloads {
        let bytes: usize = workload.transactions.iter().map(|tx| tx.len()).sum();
        let mut transactions = vec![L1_BLOCK_INFO_DEPOSIT_TX];
        transactions.extend(workload.transactions.iter().cloned());
        let wait = BUILD_WAIT_BASE + BUILD_WAIT_PER_MIB.mul_f64(bytes as f64 / (1024.0 * 1024.0));

        // Warm up both paths and check the builder kept every transaction.
        for engine in [&ipc, &in_memory] {
            let attrs = next_attributes(&transactions);
            let (_, included) = round_trip(engine, genesis_state, attrs, wait).await?;
            if included != transactions.len() {
                return Err(eyre!(
                    "{}: payload kept {included} of {} transactions",
                    workload.name,
                    transactions.len()
                ));
            }
        }

        let (mut ipc_total, mut mem_total) = (Timings::default(), Timings::default());
        for _ in 0..iters {
            for (engine, total) in [(&ipc, &mut ipc_total), (&in_memory, &mut mem_total)] {
                let attrs = next_attributes(&transactions);
                let (timings, _) = round_trip(engine, genesis_state, attrs, wait).await?;
                total.sequencer += timings.sequencer;
                total.syncing += timings.syncing;
            }
        }
        let ms = |d: Duration| d.as_secs_f64() * 1e3 / iters as f64;
        let ratio = |a: Duration, b: Duration| a.as_secs_f64() / b.as_secs_f64();
        println!(
            "| {} | {} | {:.1} | {:.2} ms | {:.2} ms | {:.1}× | {:.2} ms | {:.2} ms | {:.1}× |",
            workload.name,
            workload.transactions.len(),
            bytes as f64 / 1024.0,
            ms(ipc_total.sequencer),
            ms(mem_total.sequencer),
            ratio(ipc_total.sequencer, mem_total.sequencer),
            ms(ipc_total.syncing),
            ms(mem_total.syncing),
            ratio(ipc_total.syncing, mem_total.syncing),
        );
    }

    drop(ipc);
    drop(in_memory);
    drop(launched);
    drop(runtime);
    let _ = std::fs::remove_dir_all(data_path);
    let _ = std::fs::remove_file(ipc_path);
    Ok(())
}

/// Runs one build, fetch, and import cycle on genesis, returning its timings and how many
/// transactions the built payload holds.
async fn round_trip(
    engine: &BaseEngineClient<RootProvider, RootProvider<Base>>,
    state: ForkchoiceState,
    attributes: BasePayloadAttributes,
    build_wait: Duration,
) -> Result<(Timings, usize)> {
    let start = Instant::now();
    let updated = engine.fork_choice_updated_v3(state, Some(attributes)).await?;
    let fcu = start.elapsed();
    let payload_id =
        updated.payload_id.ok_or_else(|| eyre!("forkchoice did not start a payload"))?;
    tokio::time::sleep(build_wait).await;

    let start = Instant::now();
    let envelope = engine.get_payload_v4(payload_id).await?;
    let included =
        envelope.execution_payload.payload_inner.payload_inner.payload_inner.transactions.len();
    let status = engine
        .new_payload_v4(envelope.execution_payload, envelope.parent_beacon_block_root)
        .await?;
    let seal = start.elapsed();
    if !status.status.is_valid() {
        return Err(eyre!("payload rejected: {status:?}"));
    }
    Ok((Timings { sequencer: seal, syncing: fcu + seal }, included))
}

/// Launches a unified node with auth IPC enabled, as consensus would see it before the
/// in-memory Engine API.
async fn launch_node(
    data_path: &Path,
    ipc_path: &Path,
) -> Result<(Runtime, base_node_runner::LaunchedBaseNode)> {
    let mut genesis = build_test_genesis();
    genesis.gas_limit = BLOCK_GAS_LIMIT;
    let base_fee_params = BaseChainSpec::from_genesis(genesis.clone())
        .base_fee_params_at_timestamp(genesis.timestamp);
    // Jovian is active at genesis, so a child of genesis is validated against Jovian-encoded
    // parent extra data.
    genesis.extra_data = JovianExtraData::encode(B64::ZERO, base_fee_params, 0)?;
    let chain_spec = Arc::new(BaseChainSpec::from_genesis(genesis));

    let mut rpc = RpcServerArgs::default().with_unused_ports().with_auth_ipc();
    rpc.auth_ipc_path = ipc_path.to_string_lossy().into_owned();
    let network = NetworkArgs {
        discovery: DiscoveryArgs { disable_discovery: true, ..DiscoveryArgs::default() },
        ..NetworkArgs::default()
    };
    let node_config = NodeConfig::new(chain_spec)
        .with_network(network)
        .with_rpc(rpc)
        .with_unused_ports()
        .with_datadir_args(DatadirArgs {
            datadir: MaybePlatformPath::<DataDirPath>::from(data_path.to_path_buf()),
            ..Default::default()
        });
    let db = init_db(
        node_config.datadir().db(),
        DatabaseArguments::new(ClientVersion::default())
            .with_geometry_max_size(Some(DB_MAX_SIZE_BYTES)),
    )
    .wrap_err("failed to create benchmark database")?;

    let runtime = Runtime::test();
    let launched = BaseNodeRunner::new(RollupArgs::default())
        .launch(
            NodeBuilder::new(node_config).with_database(db).with_launch_context(runtime.clone()),
        )
        .await?;
    Ok((runtime, launched))
}

async fn engine_client(
    l2: EngineEndpoint,
) -> Result<BaseEngineClient<RootProvider, RootProvider<Base>>> {
    Ok(EngineClientBuilder {
        l2,
        l2_jwt: JwtSecret::random(),
        l1_rpc: "http://127.0.0.1:8545".parse()?,
        l1_rpc_timeout: L1_RPC_TIMEOUT,
        cfg: Arc::new(RollupConfig::default()),
    }
    .build()
    .await?)
}

/// Connects over IPC, retrying while the node creates the socket.
async fn connect_ipc(url: Url) -> Result<BaseEngineClient<RootProvider, RootProvider<Base>>> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match engine_client(EngineEndpoint::Url(url.clone())).await {
            Ok(client) => return Ok(client),
            Err(_) if Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(100)).await
            }
            Err(error) => return Err(error.wrap_err("auth IPC socket never came up")),
        }
    }
}

fn attributes(
    parent: &Header,
    timestamp: u64,
    transactions: &[Bytes],
    eip_1559_params: B64,
) -> BasePayloadAttributes {
    BasePayloadAttributes {
        payload_attributes: PayloadAttributes {
            timestamp,
            prev_randao: B256::ZERO,
            suggested_fee_recipient: parent.beneficiary,
            withdrawals: Some(vec![]),
            parent_beacon_block_root: Some(parent.parent_beacon_block_root.unwrap_or(B256::ZERO)),
            slot_number: None,
            target_gas_limit: None,
        },
        transactions: Some(transactions.to_vec()),
        no_tx_pool: Some(true),
        gas_limit: Some(parent.gas_limit),
        eip_1559_params: Some(eip_1559_params),
        min_base_fee: parent.base_fee_per_gas,
    }
}

/// Signs an EIP-1559 call from Alice with enough gas for its calldata.
fn sign(nonce: u64, to: Address, input: Bytes) -> Result<Bytes> {
    let gas_limit = BASE_TX_GAS + MAX_CALLDATA_GAS_PER_BYTE * input.len() as u64;
    let tx = BaseTransactionRequest::default()
        .from(Account::Alice.address())
        .to(to)
        .value(U256::ZERO)
        .input(input.into())
        .nonce(nonce)
        .gas_limit(gas_limit)
        .chain_id(DEVNET_CHAIN_ID)
        .max_fee_per_gas(MAX_FEE_PER_GAS_WEI)
        .max_priority_fee_per_gas(0)
        .transaction_type(2)
        .build_typed_tx()
        .map_err(|_| eyre!("invalid transaction request"))?;
    let signature = Account::Alice.signer().sign_hash_sync(&tx.signature_hash())?;
    Ok(tx.into_signed(signature).encoded_2718().into())
}

/// Deterministic, mostly non-zero calldata, so the encoded size is the worst case for JSON hex.
fn synthetic_calldata(seed: u64, len: usize) -> Bytes {
    let mut state = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state as u8
        })
        .collect()
}

/// Fetches mainnet blocks and re-signs their non-deposit transactions from Alice.
async fn mainnet_workloads(rpc: &str, spec: &str) -> Result<Vec<Workload>> {
    let provider: RootProvider<Base> =
        RootProvider::new(RpcClient::builder().http(rpc.parse().wrap_err("invalid mainnet RPC")?));
    let mut workloads = Vec::new();
    for group in spec.split(',').map(str::trim).filter(|g| !g.is_empty()) {
        let mut transactions = Vec::new();
        for number in group.split('+') {
            let number: u64 = number.trim().parse().wrap_err("invalid block number")?;
            let block = provider
                .get_block_by_number(BlockNumberOrTag::Number(number))
                .full()
                .await?
                .ok_or_else(|| eyre!("mainnet block {number} not found"))?;
            for tx in block.transactions.into_transactions() {
                if tx.ty() == DEPOSIT_TX_TYPE_ID {
                    continue;
                }
                let to = match tx.kind() {
                    TxKind::Call(to) => to,
                    TxKind::Create => Account::Bob.address(),
                };
                transactions.push(sign(transactions.len() as u64, to, tx.input().clone())?);
            }
        }
        workloads.push(Workload {
            name: format!("mainnet {}", group.replace('+', " + ")),
            transactions,
        });
    }
    Ok(workloads)
}
