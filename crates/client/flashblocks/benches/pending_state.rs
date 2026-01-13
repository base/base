#![allow(missing_docs)]

use std::{
    sync::{Arc, Once},
    time::{Duration, Instant},
};

use alloy_eips::{BlockHashOrNumber, Encodable2718};
use alloy_primitives::{Address, B256, BlockNumber, Bytes, U256, bytes, hex::FromHex};
use alloy_rpc_types_engine::PayloadId;
use base_client_node::test_utils::{Account, LocalNodeProvider, TestHarness};
use base_flashblocks::{FlashblocksAPI, FlashblocksReceiver, FlashblocksState};
use base_flashtypes::{
    ExecutionPayloadBaseV1, ExecutionPayloadFlashblockDeltaV1, Flashblock, Metadata,
};
use criterion::{BatchSize, Criterion, Throughput, criterion_group, criterion_main};
use reth_chainspec::{ChainSpecProvider, EthChainSpec};
use reth_optimism_primitives::{OpBlock, OpTransactionSigned};
use reth_primitives_traits::{Block as BlockT, RecoveredBlock};
use reth_provider::BlockReader;
use reth_transaction_pool::test_utils::TransactionBuilder;
use tokio::{runtime::Runtime, time::sleep};
use tracing_subscriber::{EnvFilter, filter::LevelFilter};

const DEPOSIT_GAS_USED: u64 = 21_000;
const TX_GAS_USED: u64 = 21_000;
const CHUNK_SIZE: usize = 10;

const BLOCK_INFO_TXN: Bytes = bytes!(
    "0x7ef90104a06c0c775b6b492bab9d7e81abdf27f77cafb698551226455a82f559e0f93fea3794deaddeaddeaddeaddeaddeaddeaddeaddead00019442000000000000000000000000000000000000158080830f424080b8b0098999be000008dd00101c1200000000000000020000000068869d6300000000015f277f000000000000000000000000000000000000000000000000000000000d42ac290000000000000000000000000000000000000000000000000000000000000001abf52777e63959936b1bf633a2a643f0da38d63deffe49452fed1bf8a44975d50000000000000000000000005050f69a9786f081509234f1a7f4684b5e5b76c9000000000000000000000000"
);

struct BenchSetup {
    provider: LocalNodeProvider,
    canonical_block: RecoveredBlock<OpBlock>,
    flashblocks: Vec<(String, Vec<Flashblock>)>,
    target_block: BlockNumber,
    _harness: Arc<TestHarness>,
}

impl BenchSetup {
    async fn new(tx_counts: &[usize]) -> Self {
        let harness =
            Arc::new(TestHarness::new().await.expect("flashblocks bench: harness should start"));
        let provider = harness.blockchain_provider();

        let canonical_block = provider
            .block(BlockHashOrNumber::Number(0))
            .expect("block lookup should succeed")
            .expect("genesis block should exist")
            .try_into_recovered()
            .expect("recovered block should build");

        let flashblocks = tx_counts
            .iter()
            .map(|count| {
                let txs = sample_transactions(&provider, *count);
                let blocks = build_flashblocks(&canonical_block, &txs);
                (format!("pending_state_{}_txs", count), blocks)
            })
            .collect();

        Self {
            target_block: canonical_block.number + 1,
            provider,
            canonical_block,
            flashblocks,
            _harness: harness,
        }
    }
}

struct BenchInput {
    provider: LocalNodeProvider,
    canonical_block: RecoveredBlock<OpBlock>,
    flashblocks: Vec<Flashblock>,
    target_block: BlockNumber,
    last_index: u64,
}

fn pending_state_benches(c: &mut Criterion) {
    init_bench_tracing();

    let runtime = Runtime::new().expect("tokio runtime should start");
    let setup = runtime.block_on(BenchSetup::new(&[5, 25, 100]));
    let mut group = c.benchmark_group("pending_state_build");

    for (label, flashblocks) in setup.flashblocks {
        let provider = setup.provider.clone();
        let canonical_block = setup.canonical_block.clone();
        let target_block = setup.target_block;
        let flashblocks = flashblocks.clone();
        let last_index = flashblocks.last().map(|fb| fb.index).unwrap_or_default();
        let tx_count = flashblocks.iter().map(|fb| fb.diff.transactions.len()).sum::<usize>();

        group.throughput(Throughput::Elements(tx_count as u64));

        group.bench_function(label, |b| {
            b.to_async(&runtime).iter_batched(
                || BenchInput {
                    provider: provider.clone(),
                    canonical_block: canonical_block.clone(),
                    flashblocks: flashblocks.clone(),
                    target_block,
                    last_index,
                },
                |input| async move { build_pending_state(input).await },
                BatchSize::SmallInput,
            )
        });
    }

    group.finish();
}

async fn build_pending_state(input: BenchInput) {
    let state = FlashblocksState::new(input.provider, 5);
    state.start();
    state.on_canonical_block_received(input.canonical_block);

    for flashblock in input.flashblocks {
        state.on_flashblock_received(flashblock);
    }

    wait_for_pending_state(&state, input.target_block, input.last_index).await;
}

fn init_bench_tracing() {
    // Keep benchmark output clean unless explicitly requested.
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        let verbose = std::env::var_os("FLASHBLOCKS_BENCH_VERBOSE").is_some();
        let default_level = if verbose { LevelFilter::INFO } else { LevelFilter::ERROR };

        let mut filter =
            EnvFilter::builder().with_default_directive(default_level.into()).from_env_lossy();

        for directive in ["reth_tasks=off", "reth_node_builder::launch::common=off"].into_iter() {
            if let Ok(directive) = directive.parse() {
                filter = filter.add_directive(directive);
            }
        }

        // Ignore errors if another subscriber was already set up.
        let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
    });
}

async fn wait_for_pending_state(
    state: &FlashblocksState<LocalNodeProvider>,
    target_block: BlockNumber,
    expected_index: u64,
) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(pending) = state.get_pending_blocks().as_ref() {
            if pending.latest_block_number() == target_block
                && pending.latest_flashblock_index() == expected_index
            {
                return;
            }
        }

        if Instant::now() > deadline {
            panic!("pending state was not built in time");
        }

        sleep(Duration::from_millis(10)).await;
    }
}

fn build_flashblocks(
    canonical_block: &RecoveredBlock<OpBlock>,
    transactions: &[OpTransactionSigned],
) -> Vec<Flashblock> {
    let mut flashblocks = Vec::new();
    let block_number = canonical_block.number + 1;

    flashblocks.push(base_flashblock(canonical_block, block_number));

    let chunk_size = CHUNK_SIZE.max(1);
    let mut gas_used = DEPOSIT_GAS_USED;
    for (idx, chunk) in transactions.chunks(chunk_size).enumerate() {
        flashblocks.push(transaction_flashblock(
            block_number,
            (idx + 1) as u64,
            chunk,
            &mut gas_used,
        ));
    }

    flashblocks
}

fn base_flashblock(
    canonical_block: &RecoveredBlock<OpBlock>,
    block_number: BlockNumber,
) -> Flashblock {
    Flashblock {
        payload_id: PayloadId::default(),
        index: 0,
        base: Some(ExecutionPayloadBaseV1 {
            parent_beacon_block_root: canonical_block.hash(),
            parent_hash: canonical_block.hash(),
            fee_recipient: Address::ZERO,
            prev_randao: B256::ZERO,
            block_number,
            gas_limit: canonical_block.gas_limit,
            timestamp: canonical_block.timestamp + 2,
            extra_data: Bytes::new(),
            base_fee_per_gas: U256::from(100),
        }),
        diff: ExecutionPayloadFlashblockDeltaV1 {
            state_root: B256::ZERO,
            receipts_root: B256::ZERO,
            block_hash: B256::ZERO,
            gas_used: DEPOSIT_GAS_USED,
            withdrawals: Vec::new(),
            logs_bloom: Default::default(),
            withdrawals_root: Default::default(),
            transactions: vec![BLOCK_INFO_TXN.clone()],
            blob_gas_used: Default::default(),
        },
        metadata: Metadata { block_number },
    }
}

fn transaction_flashblock(
    block_number: BlockNumber,
    index: u64,
    transactions: &[OpTransactionSigned],
    gas_used: &mut u64,
) -> Flashblock {
    let mut tx_bytes = Vec::with_capacity(transactions.len());

    for tx in transactions {
        *gas_used += TX_GAS_USED;
        tx_bytes.push(tx.encoded_2718().into());
    }

    Flashblock {
        payload_id: PayloadId::default(),
        index,
        base: None,
        diff: ExecutionPayloadFlashblockDeltaV1 {
            state_root: B256::ZERO,
            receipts_root: B256::ZERO,
            block_hash: B256::ZERO,
            gas_used: *gas_used,
            withdrawals: Vec::new(),
            logs_bloom: Default::default(),
            withdrawals_root: Default::default(),
            transactions: tx_bytes,
            blob_gas_used: Default::default(),
        },
        metadata: Metadata { block_number },
    }
}

fn sample_transactions(provider: &LocalNodeProvider, count: usize) -> Vec<OpTransactionSigned> {
    let signer = B256::from_hex(Account::Alice.private_key()).expect("valid private key hex");
    let chain_id = provider.chain_spec().chain_id();

    (0..count as u64)
        .map(|nonce| {
            let txn = TransactionBuilder::default()
                .signer(signer)
                .chain_id(chain_id)
                .to(Account::Bob.address())
                .nonce(nonce)
                .value(1_000_000_000u128)
                .gas_limit(TX_GAS_USED)
                .max_fee_per_gas(1_000_000_000)
                .max_priority_fee_per_gas(1_000_000_000)
                .into_eip1559()
                .as_eip1559()
                .expect("should convert to eip1559")
                .clone();

            OpTransactionSigned::Eip1559(txn)
        })
        .collect()
}

criterion_group!(benches, pending_state_benches);
criterion_main!(benches);
