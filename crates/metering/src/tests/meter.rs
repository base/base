use std::sync::Arc;

use alloy_consensus::crypto::secp256k1::public_key_to_address;
use alloy_eips::Encodable2718;
use alloy_genesis::GenesisAccount;
use alloy_primitives::{Address, B256, Bytes, U256, keccak256};
use eyre::Context;
use op_alloy_consensus::OpTxEnvelope;
use rand::{SeedableRng, rngs::StdRng};
use reth::api::NodeTypesWithDBAdapter;
use reth::chainspec::EthChainSpec;
use reth_db::{DatabaseEnv, test_utils::TempDatabase};
use reth_optimism_chainspec::{BASE_MAINNET, OpChainSpec, OpChainSpecBuilder};
use reth_optimism_node::OpNode;
use reth_optimism_primitives::OpTransactionSigned;
use reth_primitives_traits::SealedHeader;
use reth_provider::{HeaderProvider, StateProviderFactory, providers::BlockchainProvider};
use reth_testing_utils::generators::generate_keys;
use reth_transaction_pool::test_utils::TransactionBuilder;
use tips_core::types::{Bundle, BundleWithMetadata};

use super::utils::create_provider_factory;
use crate::meter_bundle;

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
    user_to_address: std::collections::HashMap<User, Address>,
    user_to_private_key: std::collections::HashMap<User, B256>,
}

impl TestHarness {
    fn address(&self, u: User) -> Address {
        self.user_to_address[&u]
    }

    fn signer(&self, u: User) -> B256 {
        self.user_to_private_key[&u]
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
            (
                alice_address,
                GenesisAccount::default().with_balance(U256::from(1_000_000_000_u64)),
            ),
            (
                bob_address,
                GenesisAccount::default().with_balance(U256::from(1_000_000_000_u64)),
            ),
        ])
        .with_gas_limit(100_000_000);

    let spec = Arc::new(
        OpChainSpecBuilder::base_mainnet()
            .genesis(genesis)
            .isthmus_activated()
            .build(),
    );

    (spec, addresses, private_keys)
}

fn setup_harness() -> eyre::Result<TestHarness> {
    let (chain_spec, user_to_address, user_to_private_key) = create_chain_spec(1337);
    let factory = create_provider_factory::<OpNode>(chain_spec.clone());

    reth_db_common::init::init_genesis(&factory).context("initializing genesis state")?;

    let provider = BlockchainProvider::new(factory.clone()).context("creating provider")?;
    let header = provider
        .sealed_header(0)
        .context("fetching genesis header")?
        .expect("genesis header exists");

    Ok(TestHarness {
        provider,
        header,
        chain_spec,
        user_to_address,
        user_to_private_key,
    })
}

fn envelope_from_signed(tx: &OpTransactionSigned) -> eyre::Result<OpTxEnvelope> {
    Ok(tx.clone().into())
}

fn create_bundle_with_metadata(envelopes: Vec<OpTxEnvelope>) -> eyre::Result<BundleWithMetadata> {
    let txs: Vec<Bytes> = envelopes
        .iter()
        .map(|env| Bytes::from(env.encoded_2718()))
        .collect();

    let bundle = Bundle {
        txs,
        block_number: 0,
        flashblock_number_min: None,
        flashblock_number_max: None,
        min_timestamp: None,
        max_timestamp: None,
        reverting_tx_hashes: vec![],
        replacement_uuid: None,
        dropping_tx_hashes: vec![],
    };

    BundleWithMetadata::load(bundle).map_err(|e| eyre::eyre!(e))
}

#[test]
fn meter_bundle_empty_transactions() -> eyre::Result<()> {
    let harness = setup_harness()?;

    let state_provider = harness
        .provider
        .state_by_block_hash(harness.header.hash())
        .context("getting state provider")?;

    let bundle_with_metadata = create_bundle_with_metadata(Vec::new())?;

    let (results, total_gas_used, total_gas_fees, bundle_hash, total_execution_time) =
        meter_bundle(
            state_provider,
            harness.chain_spec.clone(),
            Vec::new(),
            &harness.header,
            &bundle_with_metadata,
        )?;

    assert!(results.is_empty());
    assert_eq!(total_gas_used, 0);
    assert_eq!(total_gas_fees, U256::ZERO);
    // Even empty bundles have some EVM setup overhead
    assert!(total_execution_time > 0);
    assert_eq!(bundle_hash, keccak256([]));

    Ok(())
}

#[test]
fn meter_bundle_single_transaction() -> eyre::Result<()> {
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

    let envelope = envelope_from_signed(&tx)?;
    let tx_hash = envelope.tx_hash();

    let state_provider = harness
        .provider
        .state_by_block_hash(harness.header.hash())
        .context("getting state provider")?;

    let bundle_with_metadata = create_bundle_with_metadata(vec![envelope.clone()])?;

    let (results, total_gas_used, total_gas_fees, bundle_hash, total_execution_time) =
        meter_bundle(
            state_provider,
            harness.chain_spec.clone(),
            vec![envelope],
            &harness.header,
            &bundle_with_metadata,
        )?;

    assert_eq!(results.len(), 1);
    let result = &results[0];
    assert!(total_execution_time > 0);

    assert_eq!(result.from_address, harness.address(User::Alice));
    assert_eq!(result.to_address, Some(to));
    assert_eq!(result.tx_hash, tx_hash);
    assert_eq!(result.gas_price, U256::from(10).to_string());
    assert_eq!(result.gas_used, 21_000);
    assert_eq!(
        result.coinbase_diff,
        (U256::from(21_000) * U256::from(10)).to_string(),
    );

    assert_eq!(total_gas_used, 21_000);
    assert_eq!(total_gas_fees, U256::from(21_000) * U256::from(10));

    let mut concatenated = Vec::with_capacity(32);
    concatenated.extend_from_slice(tx_hash.as_slice());
    assert_eq!(bundle_hash, keccak256(concatenated));

    assert!(
        result.execution_time_us > 0,
        "execution_time_us should be greater than zero"
    );

    Ok(())
}

#[test]
fn meter_bundle_multiple_transactions() -> eyre::Result<()> {
    let harness = setup_harness()?;

    let to_1 = Address::random();
    let to_2 = Address::random();

    // Create first transaction
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
        signed_tx_1
            .as_eip1559()
            .expect("eip1559 transaction")
            .clone(),
    );

    // Create second transaction
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
        signed_tx_2
            .as_eip1559()
            .expect("eip1559 transaction")
            .clone(),
    );

    let envelope_1 = envelope_from_signed(&tx_1)?;
    let envelope_2 = envelope_from_signed(&tx_2)?;
    let tx_hash_1 = envelope_1.tx_hash();
    let tx_hash_2 = envelope_2.tx_hash();

    let state_provider = harness
        .provider
        .state_by_block_hash(harness.header.hash())
        .context("getting state provider")?;

    let bundle_with_metadata =
        create_bundle_with_metadata(vec![envelope_1.clone(), envelope_2.clone()])?;

    let (results, total_gas_used, total_gas_fees, bundle_hash, total_execution_time) =
        meter_bundle(
            state_provider,
            harness.chain_spec.clone(),
            vec![envelope_1, envelope_2],
            &harness.header,
            &bundle_with_metadata,
        )?;

    assert_eq!(results.len(), 2);
    assert!(total_execution_time > 0);

    // Check first transaction
    let result_1 = &results[0];
    assert_eq!(result_1.from_address, harness.address(User::Alice));
    assert_eq!(result_1.to_address, Some(to_1));
    assert_eq!(result_1.tx_hash, tx_hash_1);
    assert_eq!(result_1.gas_price, U256::from(10).to_string());
    assert_eq!(result_1.gas_used, 21_000);
    assert_eq!(
        result_1.coinbase_diff,
        (U256::from(21_000) * U256::from(10)).to_string(),
    );

    // Check second transaction
    let result_2 = &results[1];
    assert_eq!(result_2.from_address, harness.address(User::Bob));
    assert_eq!(result_2.to_address, Some(to_2));
    assert_eq!(result_2.tx_hash, tx_hash_2);
    assert_eq!(result_2.gas_price, U256::from(15).to_string());
    assert_eq!(result_2.gas_used, 21_000);
    assert_eq!(
        result_2.coinbase_diff,
        (U256::from(21_000) * U256::from(15)).to_string(),
    );

    // Check aggregated values
    assert_eq!(total_gas_used, 42_000);
    let expected_total_fees =
        U256::from(21_000) * U256::from(10) + U256::from(21_000) * U256::from(15);
    assert_eq!(total_gas_fees, expected_total_fees);

    // Check bundle hash includes both transactions
    let mut concatenated = Vec::with_capacity(64);
    concatenated.extend_from_slice(tx_hash_1.as_slice());
    concatenated.extend_from_slice(tx_hash_2.as_slice());
    assert_eq!(bundle_hash, keccak256(concatenated));

    assert!(
        result_1.execution_time_us > 0,
        "execution_time_us should be greater than zero"
    );
    assert!(
        result_2.execution_time_us > 0,
        "execution_time_us should be greater than zero"
    );

    Ok(())
}
