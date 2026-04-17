//! Benchmarks for state root computation latency during block building.
//!
//! Measures the **residual wait time** — the duration the builder blocks after
//! all transaction execution completes, waiting for the sparse trie task to
//! deliver the final state root.  This is the actual latency cost to block
//! finalization because the sparse trie task pipelines multiproof computation
//! alongside transaction execution.
//!
//! Each iteration:
//! 1. Spawns a `PayloadProcessor` with a warm sparse trie (matching production).
//! 2. Feeds per-flashblock state updates through the state hook with simulated
//!    execution delays, allowing the trie task to work in parallel.
//! 3. **Measures only** the time from `finish()` (drop hook) to receiving the
//!    computed state root — the residual wait.
//!
//! A cold-start warmup block is run before measurement begins.  The processor
//! is reused across iterations so the sparse trie stays warm (production
//! behavior).  50k pre-populated base-state accounts ensure the trie exercises
//! real data.
use std::{
    hint::black_box,
    sync::Arc,
    time::{Duration, Instant},
};

use alloy_consensus::constants::KECCAK_EMPTY;
use alloy_evm::block::StateChangeSource;
use alloy_primitives::{Address, B256};
use base_common_consensus::{BasePrimitives, BaseTransactionSigned};
use base_execution_evm::BaseEvmConfig;
use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use rand::{Rng, SeedableRng, rngs::StdRng};
use reth_chainspec::ChainSpec;
use reth_db_common::init::init_genesis;
use reth_engine_tree::tree::{
    ExecutionEnv, PayloadProcessor, StateProviderBuilder, TreeConfig,
    precompile_cache::PrecompileCacheMap,
};
use reth_evm::OnStateHook;
use reth_primitives_traits::{Account as RethAccount, Recovered, StorageEntry};
use reth_provider::{
    HashingWriter, ProviderFactory,
    providers::{BlockchainProvider, OverlayBuilder, OverlayStateProviderFactory},
    test_utils::{MockNodeTypesWithDB, create_test_provider_factory_with_chain_spec},
};
use reth_trie_db::ChangesetCache;
use revm::{
    primitives::U256,
    state::{Account as RevmAccount, AccountInfo, AccountStatus, EvmState, EvmStorageSlot},
};

/// Number of flashblocks per block.
const FLASHBLOCKS: usize = 10;

/// Storage slots per account.
const SLOTS_PER_ACCOUNT: usize = 5;

/// Number of pre-existing accounts in the MDBX base state.
const BASE_STATE_ACCOUNTS: usize = 50_000;

/// Simulated execution time per flashblock state update.
///
/// Base has 2-second block times with 10 flashblocks at ~200 ms intervals.
/// The sparse trie task has the full inter-flashblock gap to pipeline
/// multiproof computation alongside transaction execution.
const EXECUTION_DELAY_PER_UPDATE: Duration = Duration::from_millis(200);

/// Busy-waits for the specified duration using a spin loop.
///
/// Unlike `thread::sleep`, this keeps the current thread (and the OS
/// scheduler) fully active, preventing worker-thread descheduling artifacts
/// that would inflate the subsequent residual-wait measurement.
fn busy_wait(duration: Duration) {
    let deadline = Instant::now() + duration;
    while Instant::now() < deadline {
        std::hint::spin_loop();
    }
}

/// Generates `n` random EVM accounts with the given number of storage slots.
fn generate_evm_accounts(rng: &mut impl Rng, n: usize, slots: usize) -> EvmState {
    let mut state = EvmState::default();
    for _ in 0..n {
        let address = Address::random_with(&mut *rng);
        let account = RevmAccount {
            info: AccountInfo {
                balance: U256::from(rng.random::<u64>()),
                nonce: rng.random::<u64>(),
                code_hash: KECCAK_EMPTY,
                code: Some(Default::default()),
                account_id: None,
            },
            storage: (0..slots)
                .map(|_| {
                    (
                        U256::from(rng.random::<u64>()),
                        EvmStorageSlot::new_changed(U256::ZERO, U256::from(rng.random::<u64>()), 0),
                    )
                })
                .collect(),
            status: AccountStatus::Touched,
            original_info: Box::new(AccountInfo::default()),
            transaction_id: 0,
        };
        state.insert(address, account);
    }
    state
}

fn convert_revm_to_reth_account(revm_account: &RevmAccount) -> Option<RethAccount> {
    match revm_account.status {
        AccountStatus::SelfDestructed => None,
        _ => Some(RethAccount {
            balance: revm_account.info.balance,
            nonce: revm_account.info.nonce,
            bytecode_hash: if revm_account.info.code_hash == KECCAK_EMPTY {
                None
            } else {
                Some(revm_account.info.code_hash)
            },
        }),
    }
}

/// Pre-populates the provider factory with `EvmState` updates via
/// `HashingWriter` so that the trie contains realistic base state.
fn populate_provider(
    factory: &ProviderFactory<MockNodeTypesWithDB>,
    state: &EvmState,
) -> Result<(), Box<dyn std::error::Error>> {
    let provider_rw = factory.provider_rw()?;
    for (address, account) in state {
        provider_rw.insert_account_for_hashing(std::iter::once((
            *address,
            convert_revm_to_reth_account(account),
        )))?;
        if account.status == AccountStatus::Touched {
            let storage: Vec<StorageEntry> = account
                .storage
                .iter()
                .map(|(slot, value)| StorageEntry {
                    key: B256::from(*slot),
                    value: value.present_value,
                })
                .collect();
            provider_rw
                .insert_storage_for_hashing(std::iter::once((*address, storage.into_iter())))?;
        }
    }
    provider_rw.commit()?;
    Ok(())
}

/// Pre-generates per-flashblock `EvmState` deltas.
fn setup_flashblock_data(accounts_per_flashblock: usize) -> Vec<EvmState> {
    let mut rng = StdRng::seed_from_u64(0);
    (0..FLASHBLOCKS)
        .map(|_| generate_evm_accounts(&mut rng, accounts_per_flashblock, SLOTS_PER_ACCOUNT))
        .collect()
}

/// The tree config used throughout the benchmark.
///
/// Pruning disabled so the full sparse trie is preserved between `spawn()`
/// calls, isolating trie computation cost from cache eviction noise.
/// Production uses the default (pruning enabled) for memory efficiency.
fn bench_tree_config() -> TreeConfig {
    TreeConfig::default().with_disable_sparse_trie_cache_pruning(true)
}

/// Recovered tx entry accepted by `PayloadProcessor::spawn()`.
type BenchTx = Result<Recovered<BaseTransactionSigned>, core::convert::Infallible>;

/// Empty transaction iterator for `PayloadProcessor::spawn()`.
fn empty_txs() -> (Vec<BenchTx>, fn(BenchTx) -> BenchTx) {
    (Vec::new(), std::convert::identity)
}

fn execution_env(
    parent_hash: B256,
    parent_state_root: B256,
    transaction_count: usize,
) -> ExecutionEnv<BaseEvmConfig> {
    ExecutionEnv {
        evm_env: Default::default(),
        hash: B256::ZERO,
        parent_hash,
        parent_state_root,
        transaction_count,
        gas_used: 0,
        withdrawals: None,
        decoded_bal: None,
    }
}

/// Creates a provider factory with genesis + 50k pre-populated accounts and
/// returns a ready-to-use `PayloadProcessor` + `BlockchainProvider`.
fn setup_provider_and_processor()
-> (B256, PayloadProcessor<BaseEvmConfig>, BlockchainProvider<MockNodeTypesWithDB>) {
    let chain_spec = Arc::new(ChainSpec::default());
    let factory = create_test_provider_factory_with_chain_spec(Arc::clone(&chain_spec));
    let genesis_hash = init_genesis(&factory).unwrap();

    // Pre-populate 50k base-state accounts.
    let mut rng = StdRng::seed_from_u64(1);
    let base_state = generate_evm_accounts(&mut rng, BASE_STATE_ACCOUNTS, SLOTS_PER_ACCOUNT);
    populate_provider(&factory, &base_state).expect("failed to populate base state");

    let evm_config = BaseEvmConfig::base(Arc::new(chain_spec.as_ref().clone().into()));
    let config = bench_tree_config();
    let processor = PayloadProcessor::new(
        reth_tasks::Runtime::test(),
        evm_config,
        &config,
        PrecompileCacheMap::default(),
    );
    let provider = BlockchainProvider::new(factory).unwrap();

    (genesis_hash, processor, provider)
}

fn run_state_root_task(
    processor: &mut PayloadProcessor<BaseEvmConfig>,
    provider: &BlockchainProvider<MockNodeTypesWithDB>,
    parent_hash: B256,
    parent_state_root: B256,
    updates: &[EvmState],
    execution_delay: Option<Duration>,
) -> (B256, Duration) {
    let config = bench_tree_config();
    let env = execution_env(parent_hash, parent_state_root, updates.len());

    let mut handle = processor.spawn(
        env,
        empty_txs(),
        StateProviderBuilder::<BasePrimitives, _>::new(provider.clone(), parent_hash, None),
        OverlayStateProviderFactory::new(
            provider.clone(),
            OverlayBuilder::<BasePrimitives>::new(parent_hash, ChangesetCache::new()),
        ),
        &config,
    );

    let mut state_hook = handle.state_hook().expect("state hook disabled");
    for (i, update) in updates.iter().enumerate() {
        state_hook.on_state(StateChangeSource::Transaction(i), update);
        if let Some(delay) = execution_delay {
            busy_wait(delay);
        }
    }

    let start = Instant::now();
    drop(state_hook);
    let outcome = black_box(handle.state_root().expect("state root task failed"));

    (outcome.state_root, start.elapsed())
}

/// Benchmarks the **residual wait time**: how long the builder blocks after
/// transaction execution completes, waiting for the state root.
///
/// State updates are fed with simulated execution delays
/// ([`EXECUTION_DELAY_PER_UPDATE`] per flashblock) to allow the sparse trie
/// task to pipeline multiproof computation alongside "execution."  Only the
/// time from `drop(state_hook)` to `handle.state_root()` returning is
/// measured.
fn residual_wait_benches(c: &mut Criterion) {
    let mut g = c.benchmark_group("state_root/residual_wait");
    g.sample_size(10);

    for &accounts_per_fb in &[10, 100, 1_000] {
        let deltas = setup_flashblock_data(accounts_per_fb);

        g.bench_function(BenchmarkId::new("accounts_per_fb", accounts_per_fb), |b| {
            let (genesis_hash, mut processor, provider) = setup_provider_and_processor();
            let (mut last_root, _) = run_state_root_task(
                &mut processor,
                &provider,
                genesis_hash,
                B256::ZERO,
                &deltas,
                None,
            );

            b.iter_custom(|iters| {
                let mut total = Duration::ZERO;

                for _ in 0..iters {
                    let (state_root, elapsed) = run_state_root_task(
                        &mut processor,
                        &provider,
                        genesis_hash,
                        last_root,
                        &deltas,
                        Some(EXECUTION_DELAY_PER_UPDATE),
                    );
                    total += elapsed;
                    last_root = state_root;
                }

                total
            });
        });
    }

    g.finish();
}

criterion_group!(benches, residual_wait_benches);
criterion_main!(benches);
