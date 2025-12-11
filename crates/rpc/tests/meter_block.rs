//! Integration tests for block metering functionality.

use std::sync::Arc;

use alloy_consensus::{BlockHeader, Header, crypto::secp256k1::public_key_to_address};
use alloy_genesis::GenesisAccount;
use alloy_primitives::{Address, B256, U256};
use base_reth_rpc::meter_block;
use base_reth_test_utils::create_provider_factory;
use eyre::Context;
use rand::{SeedableRng, rngs::StdRng};
use reth::{api::NodeTypesWithDBAdapter, chainspec::EthChainSpec};
use reth_db::{DatabaseEnv, test_utils::TempDatabase};
use reth_optimism_chainspec::{BASE_MAINNET, OpChainSpec, OpChainSpecBuilder};
use reth_optimism_node::OpNode;
use reth_optimism_primitives::{OpBlock, OpBlockBody, OpTransactionSigned};
use reth_primitives_traits::{Block as BlockT, SealedHeader};
use reth_provider::{HeaderProvider, StateProviderFactory, providers::BlockchainProvider};
use reth_testing_utils::generators::generate_keys;
use reth_transaction_pool::test_utils::TransactionBuilder;

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
    chain_spec: Arc<OpChainSpec>,
    user_to_private_key: std::collections::HashMap<User, B256>,
}

impl TestHarness {
    fn signer(&self, u: User) -> B256 {
        self.user_to_private_key[&u]
    }
}

fn create_chain_spec(seed: u64) -> (Arc<OpChainSpec>, std::collections::HashMap<User, B256>) {
    let keys = generate_keys(&mut StdRng::seed_from_u64(seed), 2);

    let mut private_keys = std::collections::HashMap::new();

    let alice_key = keys[0];
    let alice_address = public_key_to_address(alice_key.public_key());
    let alice_secret = B256::from(alice_key.secret_bytes());
    private_keys.insert(User::Alice, alice_secret);

    let bob_key = keys[1];
    let bob_address = public_key_to_address(bob_key.public_key());
    let bob_secret = B256::from(bob_key.secret_bytes());
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

    (spec, private_keys)
}

fn setup_harness() -> eyre::Result<TestHarness> {
    let (chain_spec, user_to_private_key) = create_chain_spec(1337);
    let factory = create_provider_factory::<OpNode>(chain_spec.clone());

    reth_db_common::init::init_genesis(&factory).context("initializing genesis state")?;

    let provider = BlockchainProvider::new(factory.clone()).context("creating provider")?;
    let header = provider
        .sealed_header(0)
        .context("fetching genesis header")?
        .expect("genesis header exists");

    Ok(TestHarness { provider, header, chain_spec, user_to_private_key })
}

fn create_block_with_transactions(
    harness: &TestHarness,
    transactions: Vec<OpTransactionSigned>,
) -> OpBlock {
    let header = Header {
        parent_hash: harness.header.hash(),
        number: harness.header.number() + 1,
        timestamp: harness.header.timestamp() + 2,
        gas_limit: 30_000_000,
        beneficiary: Address::random(),
        base_fee_per_gas: Some(1),
        // Required for post-Cancun blocks (EIP-4788)
        parent_beacon_block_root: Some(B256::ZERO),
        ..Default::default()
    };

    let body = OpBlockBody { transactions, ommers: vec![], withdrawals: None };

    OpBlock::new(header, body)
}

#[test]
fn meter_block_empty_transactions() -> eyre::Result<()> {
    let harness = setup_harness()?;

    let block = create_block_with_transactions(&harness, vec![]);

    let state_provider = harness
        .provider
        .state_by_block_hash(harness.header.hash())
        .context("getting state provider")?;

    let response =
        meter_block(state_provider, harness.chain_spec.clone(), &block, &harness.header)?;

    assert_eq!(response.block_hash, block.header().hash_slow());
    assert_eq!(response.block_number, block.header().number());
    assert!(response.transactions.is_empty());
    // No transactions means no signer recovery
    assert_eq!(response.signer_recovery_time_us, 0);
    assert!(response.execution_time_us > 0, "execution time should be non-zero due to EVM setup");
    assert!(response.state_root_time_us > 0, "state root time should be non-zero");
    assert_eq!(
        response.total_time_us,
        response.signer_recovery_time_us + response.execution_time_us + response.state_root_time_us
    );

    Ok(())
}

#[test]
fn meter_block_single_transaction() -> eyre::Result<()> {
    let harness = setup_harness()?;

    let to = Address::random();
    let signed_tx = TransactionBuilder::default()
        .signer(harness.signer(User::Alice))
        .chain_id(harness.chain_spec.chain_id())
        .nonce(0)
        .to(to)
        .value(1_000)
        .gas_limit(21_000)
        .max_fee_per_gas(10)
        .max_priority_fee_per_gas(1)
        .into_eip1559();

    let tx =
        OpTransactionSigned::Eip1559(signed_tx.as_eip1559().expect("eip1559 transaction").clone());
    let tx_hash = tx.tx_hash();

    let block = create_block_with_transactions(&harness, vec![tx]);

    let state_provider = harness
        .provider
        .state_by_block_hash(harness.header.hash())
        .context("getting state provider")?;

    let response =
        meter_block(state_provider, harness.chain_spec.clone(), &block, &harness.header)?;

    assert_eq!(response.block_hash, block.header().hash_slow());
    assert_eq!(response.block_number, block.header().number());
    assert_eq!(response.transactions.len(), 1);

    let metered_tx = &response.transactions[0];
    assert_eq!(metered_tx.tx_hash, tx_hash);
    assert_eq!(metered_tx.gas_used, 21_000);
    assert!(metered_tx.execution_time_us > 0, "transaction execution time should be non-zero");

    assert!(response.signer_recovery_time_us > 0, "signer recovery should take time");
    assert!(response.execution_time_us > 0);
    assert!(response.state_root_time_us > 0);
    assert_eq!(
        response.total_time_us,
        response.signer_recovery_time_us + response.execution_time_us + response.state_root_time_us
    );

    Ok(())
}

#[test]
fn meter_block_multiple_transactions() -> eyre::Result<()> {
    let harness = setup_harness()?;

    let to_1 = Address::random();
    let to_2 = Address::random();

    // Create first transaction from Alice
    let signed_tx_1 = TransactionBuilder::default()
        .signer(harness.signer(User::Alice))
        .chain_id(harness.chain_spec.chain_id())
        .nonce(0)
        .to(to_1)
        .value(1_000)
        .gas_limit(21_000)
        .max_fee_per_gas(10)
        .max_priority_fee_per_gas(1)
        .into_eip1559();

    let tx_1 = OpTransactionSigned::Eip1559(
        signed_tx_1.as_eip1559().expect("eip1559 transaction").clone(),
    );
    let tx_hash_1 = tx_1.tx_hash();

    // Create second transaction from Bob
    let signed_tx_2 = TransactionBuilder::default()
        .signer(harness.signer(User::Bob))
        .chain_id(harness.chain_spec.chain_id())
        .nonce(0)
        .to(to_2)
        .value(2_000)
        .gas_limit(21_000)
        .max_fee_per_gas(15)
        .max_priority_fee_per_gas(2)
        .into_eip1559();

    let tx_2 = OpTransactionSigned::Eip1559(
        signed_tx_2.as_eip1559().expect("eip1559 transaction").clone(),
    );
    let tx_hash_2 = tx_2.tx_hash();

    let block = create_block_with_transactions(&harness, vec![tx_1, tx_2]);

    let state_provider = harness
        .provider
        .state_by_block_hash(harness.header.hash())
        .context("getting state provider")?;

    let response =
        meter_block(state_provider, harness.chain_spec.clone(), &block, &harness.header)?;

    assert_eq!(response.block_hash, block.header().hash_slow());
    assert_eq!(response.block_number, block.header().number());
    assert_eq!(response.transactions.len(), 2);

    // Check first transaction
    let metered_tx_1 = &response.transactions[0];
    assert_eq!(metered_tx_1.tx_hash, tx_hash_1);
    assert_eq!(metered_tx_1.gas_used, 21_000);
    assert!(metered_tx_1.execution_time_us > 0);

    // Check second transaction
    let metered_tx_2 = &response.transactions[1];
    assert_eq!(metered_tx_2.tx_hash, tx_hash_2);
    assert_eq!(metered_tx_2.gas_used, 21_000);
    assert!(metered_tx_2.execution_time_us > 0);

    // Check aggregate times
    assert!(response.signer_recovery_time_us > 0, "signer recovery should take time");
    assert!(response.execution_time_us > 0);
    assert!(response.state_root_time_us > 0);
    assert_eq!(
        response.total_time_us,
        response.signer_recovery_time_us + response.execution_time_us + response.state_root_time_us
    );

    // Ensure individual transaction times are consistent with total
    let individual_times: u128 = response.transactions.iter().map(|t| t.execution_time_us).sum();
    assert!(
        individual_times <= response.execution_time_us,
        "sum of individual times should not exceed total (due to EVM overhead)"
    );

    Ok(())
}

#[test]
fn meter_block_timing_consistency() -> eyre::Result<()> {
    let harness = setup_harness()?;

    // Create a block with one transaction
    let signed_tx = TransactionBuilder::default()
        .signer(harness.signer(User::Alice))
        .chain_id(harness.chain_spec.chain_id())
        .nonce(0)
        .to(Address::random())
        .value(1_000)
        .gas_limit(21_000)
        .max_fee_per_gas(10)
        .max_priority_fee_per_gas(1)
        .into_eip1559();

    let tx =
        OpTransactionSigned::Eip1559(signed_tx.as_eip1559().expect("eip1559 transaction").clone());

    let block = create_block_with_transactions(&harness, vec![tx]);

    let state_provider = harness
        .provider
        .state_by_block_hash(harness.header.hash())
        .context("getting state provider")?;

    let response =
        meter_block(state_provider, harness.chain_spec.clone(), &block, &harness.header)?;

    // Verify timing invariants
    assert!(response.signer_recovery_time_us > 0, "signer recovery time must be positive");
    assert!(response.execution_time_us > 0, "execution time must be positive");
    assert!(response.state_root_time_us > 0, "state root time must be positive");
    assert_eq!(
        response.total_time_us,
        response.signer_recovery_time_us + response.execution_time_us + response.state_root_time_us,
        "total time must equal signer recovery + execution + state root times"
    );

    Ok(())
}
