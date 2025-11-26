// =============================================================================
// Database Layering Tests
//
// These tests verify that the three-layer state architecture is correct:
//   1. StateProviderDatabase (canonical block base state)
//   2. CacheDB (applies flashblock pending read cache)
//   3. State wrapper (applies bundle_prestate for pending writes + tracks new changes)
//
// The correct layering is State<CacheDB<StateProviderDatabase>>. Writes go
// through State first (properly tracked for bundle/state root calculation),
// and reads fall through to CacheDB (flashblock read cache) then to
// StateProviderDatabase (canonical state).
//
// The WRONG layering would be CacheDB<State<...>>. CacheDB intercepts all
// writes into its internal cache and doesn't propagate them to State, so
// State's bundle tracking captures nothing. See the test
// layering_cachedb_wrapping_state_loses_writes for a demonstration.
// =============================================================================

use std::sync::Arc;

use alloy_consensus::crypto::secp256k1::public_key_to_address;
use alloy_genesis::GenesisAccount;
use alloy_primitives::{Address, B256, U256};
use eyre::Context;
use rand::{SeedableRng, rngs::StdRng};
use reth::api::NodeTypesWithDBAdapter;
use reth::revm::db::{AccountState, BundleState, Cache, CacheDB, DbAccount, State};
use reth_db::{
    ClientVersion, DatabaseEnv, init_db,
    mdbx::{DatabaseArguments, KILOBYTE, MEGABYTE, MaxReadTransactionDuration},
    test_utils::{ERROR_DB_CREATION, TempDatabase, create_test_static_files_dir, tempdir_path},
};
use reth_optimism_chainspec::{BASE_MAINNET, OpChainSpec, OpChainSpecBuilder};
use reth_optimism_node::OpNode;
use reth_primitives_traits::SealedHeader;
use reth_provider::{
    HeaderProvider, ProviderFactory, StateProviderFactory, providers::{BlockchainProvider, StaticFileProvider},
};
use reth_testing_utils::generators::generate_keys;
use revm::Database;
use revm::primitives::KECCAK_EMPTY;

type NodeTypes = NodeTypesWithDBAdapter<OpNode, Arc<TempDatabase<DatabaseEnv>>>;

#[derive(Eq, PartialEq, Debug, Hash, Clone, Copy)]
enum User {
    Alice,
    Bob,
}

#[derive(Debug, Clone)]
struct TestHarness {
    provider: BlockchainProvider<NodeTypes>,
    header: SealedHeader,
    #[allow(dead_code)]
    chain_spec: Arc<OpChainSpec>,
    user_to_address: std::collections::HashMap<User, Address>,
    #[allow(dead_code)]
    user_to_private_key: std::collections::HashMap<User, B256>,
}

impl TestHarness {
    fn address(&self, u: User) -> Address {
        self.user_to_address[&u]
    }
}

fn create_chain_spec(
    seed: u64,
) -> (
    Arc<OpChainSpec>,
    std::collections::HashMap<User, Address>,
    std::collections::HashMap<User, B256>,
) {
    let keys = generate_keys(&mut StdRng::seed_from_u64(seed), 2);

    let mut addresses = std::collections::HashMap::new();
    let mut private_keys = std::collections::HashMap::new();

    let alice_key = keys[0];
    let alice_address = public_key_to_address(alice_key.public_key());
    let alice_secret = B256::from(alice_key.secret_bytes());
    addresses.insert(User::Alice, alice_address);
    private_keys.insert(User::Alice, alice_secret);

    let bob_key = keys[1];
    let bob_address = public_key_to_address(bob_key.public_key());
    let bob_secret = B256::from(bob_key.secret_bytes());
    addresses.insert(User::Bob, bob_address);
    private_keys.insert(User::Bob, bob_secret);

    let genesis = BASE_MAINNET
        .genesis
        .clone()
        .extend_accounts(vec![
            (alice_address, GenesisAccount::default().with_balance(U256::from(1_000_000_000_u64))),
            (bob_address, GenesisAccount::default().with_balance(U256::from(1_000_000_000_u64))),
        ])
        .with_gas_limit(100_000_000);

    let spec =
        Arc::new(OpChainSpecBuilder::base_mainnet().genesis(genesis).isthmus_activated().build());

    (spec, addresses, private_keys)
}

fn create_provider_factory(
    chain_spec: Arc<OpChainSpec>,
) -> ProviderFactory<NodeTypesWithDBAdapter<OpNode, Arc<TempDatabase<DatabaseEnv>>>> {
    let (static_dir, _) = create_test_static_files_dir();
    let db = create_test_db();
    ProviderFactory::new(
        db,
        chain_spec,
        StaticFileProvider::read_write(static_dir.keep()).expect("static file provider"),
    )
}

fn create_test_db() -> Arc<TempDatabase<DatabaseEnv>> {
    let path = tempdir_path();
    let emsg = format!("{ERROR_DB_CREATION}: {path:?}");

    let db = init_db(
        &path,
        DatabaseArguments::new(ClientVersion::default())
            .with_max_read_transaction_duration(Some(MaxReadTransactionDuration::Unbounded))
            .with_geometry_max_size(Some(4 * MEGABYTE))
            .with_growth_step(Some(4 * KILOBYTE)),
    )
    .expect(&emsg);

    Arc::new(TempDatabase::new(db, path))
}

fn setup_harness() -> eyre::Result<TestHarness> {
    let (chain_spec, user_to_address, user_to_private_key) = create_chain_spec(1337);
    let factory = create_provider_factory(chain_spec.clone());

    reth_db_common::init::init_genesis(&factory).context("initializing genesis state")?;

    let provider = BlockchainProvider::new(factory.clone()).context("creating provider")?;
    let header = provider
        .sealed_header(0)
        .context("fetching genesis header")?
        .expect("genesis header exists");

    Ok(TestHarness { provider, header, chain_spec, user_to_address, user_to_private_key })
}

/// Demonstrates that State<StateProviderDatabase> alone cannot see pending state.
///
/// Without CacheDB or bundle_prestate, State can only see canonical block state.
#[test]
fn layering_old_state_only_cannot_see_pending_state() -> eyre::Result<()> {
    let harness = setup_harness()?;
    let alice_address = harness.address(User::Alice);

    let state_provider = harness
        .provider
        .state_by_block_hash(harness.header.hash())
        .context("getting state provider")?;

    // OLD implementation: just State wrapping StateProviderDatabase directly
    // No CacheDB, no bundle_prestate - cannot see any pending flashblock state
    let state_db = reth::revm::database::StateProviderDatabase::new(state_provider);
    let mut db = State::builder().with_database(state_db).with_bundle_update().build();

    // Read through State - can only see canonical state (nonce 0)
    let account = db.basic(alice_address)?.expect("account should exist");

    // Old implementation sees canonical nonce (0), not any pending state
    assert_eq!(
        account.nonce, 0,
        "Old State-only layering can only see canonical state"
    );

    Ok(())
}

/// Demonstrates that CacheDB<State<...>> is the WRONG layering order.
///
/// CacheDB is a read-through cache. When the EVM writes state changes, those
/// writes go into CacheDB's internal cache and are NOT propagated to the
/// underlying State. As a result, State's bundle tracking captures nothing,
/// and take_bundle() returns an empty bundle - breaking state root calculation.
///
/// The correct layering is State<CacheDB<...>> where State wraps CacheDB,
/// so all writes go through State first and are properly tracked.
#[test]
fn layering_cachedb_wrapping_state_loses_writes() -> eyre::Result<()> {
    use revm::DatabaseCommit;

    let harness = setup_harness()?;
    let alice_address = harness.address(User::Alice);
    let bob_address = harness.address(User::Bob);

    // =========================================================================
    // WRONG layering: CacheDB<State<StateProviderDatabase>>
    // =========================================================================
    let state_provider = harness
        .provider
        .state_by_block_hash(harness.header.hash())
        .context("getting state provider")?;

    let state_db = reth::revm::database::StateProviderDatabase::new(state_provider);
    let state = State::builder().with_database(state_db).with_bundle_update().build();
    let mut wrong_db = CacheDB::new(state);

    // Simulate a write: modify Alice's account through the CacheDB
    let mut alice_account = wrong_db.basic(alice_address)?.expect("alice should exist");
    alice_account.nonce = 999;
    alice_account.balance = U256::from(12345u64);

    // Commit the change through CacheDB
    let mut state_changes = revm::state::EvmState::default();
    state_changes.insert(
        alice_address,
        revm::state::Account {
            info: alice_account.clone(),
            storage: Default::default(),
            status: revm::state::AccountStatus::Touched,
            transaction_id: 0,
        },
    );
    wrong_db.commit(state_changes);

    // The write went into CacheDB's cache - verify we can read it back
    let alice_from_cache = wrong_db.basic(alice_address)?.expect("alice should exist");
    assert_eq!(alice_from_cache.nonce, 999, "CacheDB should have the written nonce");

    // BUT: The underlying State captured NOTHING!
    // CacheDB doesn't propagate writes to its underlying database.
    wrong_db.db.merge_transitions(revm_database::states::bundle_state::BundleRetention::Reverts);
    let wrong_bundle = wrong_db.db.take_bundle();

    assert!(
        wrong_bundle.state.is_empty(),
        "WRONG layering: State's bundle should be EMPTY because CacheDB intercepted all writes. \
         Got {} accounts in bundle.",
        wrong_bundle.state.len()
    );

    // =========================================================================
    // CORRECT layering: State<CacheDB<StateProviderDatabase>>
    // =========================================================================
    let state_provider2 = harness
        .provider
        .state_by_block_hash(harness.header.hash())
        .context("getting state provider")?;

    let state_db2 = reth::revm::database::StateProviderDatabase::new(state_provider2);
    let cache_db = CacheDB::new(state_db2);
    let mut correct_db = State::builder().with_database(cache_db).with_bundle_update().build();

    // Simulate the same write through State
    let mut bob_account = correct_db.basic(bob_address)?.expect("bob should exist");
    bob_account.nonce = 888;
    bob_account.balance = U256::from(54321u64);

    // Commit through State
    let mut state_changes2 = revm::state::EvmState::default();
    state_changes2.insert(
        bob_address,
        revm::state::Account {
            info: bob_account.clone(),
            storage: Default::default(),
            status: revm::state::AccountStatus::Touched,
            transaction_id: 0,
        },
    );
    correct_db.commit(state_changes2);

    // State properly captures the write
    correct_db.merge_transitions(revm_database::states::bundle_state::BundleRetention::Reverts);
    let correct_bundle = correct_db.take_bundle();

    assert!(
        !correct_bundle.state.is_empty(),
        "CORRECT layering: State's bundle should contain the written account"
    );
    assert!(
        correct_bundle.state.contains_key(&bob_address),
        "Bundle should contain Bob's account changes"
    );

    Ok(())
}

/// Verifies that CacheDB layer is required for pending balance visibility.
///
/// This test demonstrates that without the CacheDB layer, pending balance
/// changes from flashblocks would not be visible to bundle execution.
#[test]
fn layering_cachedb_makes_pending_balance_visible() -> eyre::Result<()> {
    let harness = setup_harness()?;

    // Get the canonical balance for Alice from the state provider
    let state_provider = harness
        .provider
        .state_by_block_hash(harness.header.hash())
        .context("getting state provider")?;

    let alice_address = harness.address(User::Alice);
    let canonical_balance = state_provider.account_balance(&alice_address)?.unwrap_or(U256::ZERO);

    // Create a cache with a modified balance (simulating a pending flashblock)
    let pending_balance = canonical_balance + U256::from(999_999_u64);
    let mut cache = Cache::default();
    cache.accounts.insert(
        alice_address,
        DbAccount {
            info: revm::state::AccountInfo {
                balance: pending_balance,
                nonce: 0,
                code_hash: KECCAK_EMPTY,
                code: None,
            },
            account_state: AccountState::Touched,
            storage: Default::default(),
        },
    );

    // Wrap with CacheDB to apply the pending cache
    let state_db = reth::revm::database::StateProviderDatabase::new(state_provider);
    let mut cache_db = CacheDB { cache, db: state_db };

    // Read the balance through CacheDB - should see pending value
    let account = cache_db.basic(alice_address)?.expect("account should exist");
    assert_eq!(
        account.balance, pending_balance,
        "CacheDB should return pending balance from cache"
    );

    // Verify the canonical state still has the original balance
    let state_provider2 = harness
        .provider
        .state_by_block_hash(harness.header.hash())
        .context("getting state provider")?;
    let canonical_balance2 =
        state_provider2.account_balance(&alice_address)?.unwrap_or(U256::ZERO);
    assert_eq!(
        canonical_balance, canonical_balance2,
        "Canonical state should be unchanged"
    );

    Ok(())
}

/// Verifies that bundle_prestate is required for pending state changes visibility.
///
/// This test demonstrates that without with_bundle_prestate(), the State wrapper
/// would start with empty state and not see pending flashblock state changes.
#[test]
fn layering_bundle_prestate_makes_pending_nonce_visible() -> eyre::Result<()> {
    let harness = setup_harness()?;
    let alice_address = harness.address(User::Alice);

    let state_provider = harness
        .provider
        .state_by_block_hash(harness.header.hash())
        .context("getting state provider")?;

    // Create a BundleState with a modified nonce (simulating pending flashblock writes)
    let pending_nonce = 42u64;
    let bundle_state = BundleState::new(
        [(
            alice_address,
            Some(revm::state::AccountInfo {
                balance: U256::from(1_000_000_000u64),
                nonce: 0, // original
                code_hash: KECCAK_EMPTY,
                code: None,
            }),
            Some(revm::state::AccountInfo {
                balance: U256::from(1_000_000_000u64),
                nonce: pending_nonce, // pending
                code_hash: KECCAK_EMPTY,
                code: None,
            }),
            Default::default(),
        )],
        Vec::<Vec<(Address, Option<Option<revm::state::AccountInfo>>, Vec<(U256, U256)>)>>::new(),
        Vec::<(B256, revm::bytecode::Bytecode)>::new(),
    );

    // Create the three-layer stack WITH bundle_prestate
    let state_db = reth::revm::database::StateProviderDatabase::new(state_provider);
    let cache_db = CacheDB::new(state_db);
    let mut db_with_prestate = State::builder()
        .with_database(cache_db)
        .with_bundle_update()
        .with_bundle_prestate(bundle_state.clone())
        .build();

    // Read through the State - should see pending nonce from bundle_prestate
    let account = db_with_prestate.basic(alice_address)?.expect("account should exist");
    assert_eq!(account.nonce, pending_nonce, "State with bundle_prestate should see pending nonce");

    // Now create WITHOUT bundle_prestate to show the difference
    let state_provider2 = harness
        .provider
        .state_by_block_hash(harness.header.hash())
        .context("getting state provider")?;
    let state_db2 = reth::revm::database::StateProviderDatabase::new(state_provider2);
    let cache_db2 = CacheDB::new(state_db2);
    let mut db_without_prestate =
        State::builder().with_database(cache_db2).with_bundle_update().build();

    // Read through State without prestate - should see canonical nonce (0)
    let account2 = db_without_prestate.basic(alice_address)?.expect("account should exist");
    assert_eq!(
        account2.nonce, 0,
        "State without bundle_prestate should see canonical nonce"
    );

    Ok(())
}
