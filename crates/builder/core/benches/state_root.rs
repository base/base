/// Benchmarks for state root computation during block building.
///
/// Two modes are benchmarked, each with 10 flashblocks at varying
/// accounts-per-flashblock sizes:
///
/// - `finalize_only` — state root computed once after all flashblocks are
///   accumulated. Matches the current production flow where
///   `calculate_state_root` is false for intermediate flashblocks.
///
/// - `per_flashblock` — state root computed after every flashblock, threading
///   `TrieUpdates` forward via [`compute_state_root`]. Currently each call
///   recomputes from scratch. Once incremental trie caching is wired through
///   `compute_state_root`, `per_flashblock` should approach `finalize_only`.
use alloy_primitives::{B256, U256, keccak256};
use base_builder_core::compute_state_root;
use base_execution_trie::{
    InMemoryProofsStorage, OpProofsStorage, provider::OpProofsStateProviderRef,
};
use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use rand::{Rng, SeedableRng, rngs::StdRng};
use reth_primitives_traits::Account;
use reth_provider::noop::NoopProvider;
use reth_trie_common::{HashedPostState, HashedStorage};

/// Number of flashblocks per block.
const FLASHBLOCKS: usize = 10;

/// Storage slots per account.
const SLOTS_PER_ACCOUNT: usize = 5;

/// Generates `n` deterministic (`hashed_address`, account, `storage_slots`) tuples.
fn generate_accounts(
    rng: &mut StdRng,
    n: usize,
    slots_per_account: usize,
) -> Vec<(B256, Account, Vec<(B256, U256)>)> {
    (0..n)
        .map(|_| {
            let addr = keccak256(rng.random::<[u8; 20]>());
            let account = Account {
                nonce: rng.random::<u64>(),
                balance: U256::from(rng.random::<u64>()),
                bytecode_hash: None,
            };
            let slots: Vec<(B256, U256)> = (0..slots_per_account)
                .map(|_| (keccak256(rng.random::<[u8; 32]>()), U256::from(rng.random::<u64>())))
                .collect();
            (addr, account, slots)
        })
        .collect()
}

/// Builds a [`HashedPostState`] from generated accounts.
fn build_hashed_post_state(accounts: &[(B256, Account, Vec<(B256, U256)>)]) -> HashedPostState {
    let mut state = HashedPostState::default();
    for (addr, account, slots) in accounts {
        state.accounts.insert(*addr, Some(*account));
        let mut storage = HashedStorage::new(false);
        for (slot, value) in slots {
            storage.storage.insert(*slot, *value);
        }
        state.storages.insert(*addr, storage);
    }
    state
}

/// Pre-generates per-flashblock account changesets and the full accumulated state.
fn setup_flashblock_data(
    accounts_per_flashblock: usize,
) -> (Vec<HashedPostState>, HashedPostState) {
    let mut rng = StdRng::seed_from_u64(42);
    let mut deltas = Vec::with_capacity(FLASHBLOCKS);
    let mut full_state = HashedPostState::default();

    for _ in 0..FLASHBLOCKS {
        let accounts = generate_accounts(&mut rng, accounts_per_flashblock, SLOTS_PER_ACCOUNT);
        let delta = build_hashed_post_state(&accounts);
        full_state.extend(delta.clone());
        deltas.push(delta);
    }

    (deltas, full_state)
}

/// Benchmarks state root computed once at finalization over the full
/// accumulated state (current production behavior).
fn finalize_only_benches(c: &mut Criterion) {
    let mut g = c.benchmark_group("state_root/finalize_only");
    g.sample_size(10);

    for &accounts_per_fb in &[10, 100, 1_000] {
        let (_deltas, full_state) = setup_flashblock_data(accounts_per_fb);
        let storage = OpProofsStorage::from(InMemoryProofsStorage::new());
        let provider =
            OpProofsStateProviderRef::new(Box::new(NoopProvider::default()), &storage, 0);

        g.bench_function(BenchmarkId::new("accounts_per_fb", accounts_per_fb), |b| {
            b.iter(|| {
                compute_state_root(&provider, full_state.clone(), None)
                    .expect("state root should succeed")
            });
        });
    }

    g.finish();
}

/// Benchmarks state root computed after every flashblock, threading
/// `TrieUpdates` forward. Currently recomputes from scratch each time.
fn per_flashblock_benches(c: &mut Criterion) {
    let mut g = c.benchmark_group("state_root/per_flashblock");
    g.sample_size(10);

    for &accounts_per_fb in &[10, 100, 1_000] {
        let (deltas, _full_state) = setup_flashblock_data(accounts_per_fb);

        g.bench_function(BenchmarkId::new("accounts_per_fb", accounts_per_fb), |b| {
            b.iter(|| {
                let storage = OpProofsStorage::from(InMemoryProofsStorage::new());
                let provider =
                    OpProofsStateProviderRef::new(Box::new(NoopProvider::default()), &storage, 0);

                let mut accumulated = HashedPostState::default();
                let mut prev_trie_updates = None;
                for delta in &deltas {
                    accumulated.extend(delta.clone());
                    let (_root, trie_updates) = compute_state_root(
                        &provider,
                        accumulated.clone(),
                        prev_trie_updates.as_ref(),
                    )
                    .expect("state root should succeed");
                    prev_trie_updates = Some(trie_updates);
                }
            });
        });
    }

    g.finish();
}

criterion_group!(benches, finalize_only_benches, per_flashblock_benches);
criterion_main!(benches);
