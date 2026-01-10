//! Integration tests covering the Metering logic surface area.

use std::sync::Arc;

use alloy_eips::Encodable2718;
use alloy_genesis::GenesisAccount;
use alloy_primitives::{Address, B256, Bytes, U256, hex::FromHex, keccak256};
use base_bundles::{Bundle, ParsedBundle};
use base_metering::meter_bundle;
use base_test_utils::{Account, create_provider_factory};
use eyre::Context;
use op_alloy_consensus::OpTxEnvelope;
use reth::{api::NodeTypesWithDBAdapter, chainspec::EthChainSpec};
use reth_db::{DatabaseEnv, test_utils::TempDatabase};
use reth_optimism_chainspec::{BASE_MAINNET, OpChainSpec, OpChainSpecBuilder};
use reth_optimism_node::OpNode;
use reth_optimism_primitives::OpTransactionSigned;
use reth_primitives_traits::SealedHeader;
use reth_provider::{HeaderProvider, StateProviderFactory, providers::BlockchainProvider};
use reth_transaction_pool::test_utils::TransactionBuilder;

type NodeTypes = NodeTypesWithDBAdapter<OpNode, Arc<TempDatabase<DatabaseEnv>>>;

#[derive(Debug, Clone)]
struct TestHarness {
    provider: BlockchainProvider<NodeTypes>,
    header: SealedHeader,
    chain_spec: Arc<OpChainSpec>,
}

impl TestHarness {
    fn signer(&self, account: Account) -> B256 {
        B256::from_hex(account.private_key()).expect("valid private key hex")
    }
}

fn create_chain_spec() -> Arc<OpChainSpec> {
    let genesis = BASE_MAINNET
        .genesis
        .clone()
        .extend_accounts(
            Account::all()
                .into_iter()
                .map(|a| {
                    (
                        a.address(),
                        GenesisAccount::default().with_balance(U256::from(1_000_000_000_u64)),
                    )
                })
                .collect::<Vec<_>>(),
        )
        .with_gas_limit(100_000_000);

    Arc::new(OpChainSpecBuilder::base_mainnet().genesis(genesis).isthmus_activated().build())
}

fn setup_harness() -> eyre::Result<TestHarness> {
    let chain_spec = create_chain_spec();
    let factory = create_provider_factory::<OpNode>(chain_spec.clone());

    reth_db_common::init::init_genesis(&factory).context("initializing genesis state")?;

    let provider = BlockchainProvider::new(factory.clone()).context("creating provider")?;
    let header = provider
        .sealed_header(0)
        .context("fetching genesis header")?
        .expect("genesis header exists");

    Ok(TestHarness { provider, header, chain_spec })
}

fn envelope_from_signed(tx: &OpTransactionSigned) -> eyre::Result<OpTxEnvelope> {
    Ok(tx.clone().into())
}

fn create_parsed_bundle(envelopes: Vec<OpTxEnvelope>) -> eyre::Result<ParsedBundle> {
    let txs: Vec<Bytes> = envelopes.iter().map(|env| Bytes::from(env.encoded_2718())).collect();

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

    ParsedBundle::try_from(bundle).map_err(|e| eyre::eyre!(e))
}

#[test]
fn meter_bundle_empty_transactions() -> eyre::Result<()> {
    let harness = setup_harness()?;

    let state_provider = harness
        .provider
        .state_by_block_hash(harness.header.hash())
        .context("getting state provider")?;

    let parsed_bundle = create_parsed_bundle(Vec::new())?;

    let (results, total_gas_used, total_gas_fees, bundle_hash, total_execution_time) =
        meter_bundle(state_provider, harness.chain_spec.clone(), parsed_bundle, &harness.header)?;

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
        .signer(harness.signer(Account::Alice))
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

    let parsed_bundle = create_parsed_bundle(vec![envelope.clone()])?;

    let (results, total_gas_used, total_gas_fees, bundle_hash, total_execution_time) =
        meter_bundle(state_provider, harness.chain_spec.clone(), parsed_bundle, &harness.header)?;

    assert_eq!(results.len(), 1);
    let result = &results[0];
    assert!(total_execution_time > 0);

    assert_eq!(result.from_address, Account::Alice.address());
    assert_eq!(result.to_address, Some(to));
    assert_eq!(result.tx_hash, tx_hash);
    assert_eq!(result.gas_price, U256::from(10));
    assert_eq!(result.gas_used, 21_000);
    assert_eq!(result.coinbase_diff, (U256::from(21_000) * U256::from(10)),);

    assert_eq!(total_gas_used, 21_000);
    assert_eq!(total_gas_fees, U256::from(21_000) * U256::from(10));

    let mut concatenated = Vec::with_capacity(32);
    concatenated.extend_from_slice(tx_hash.as_slice());
    assert_eq!(bundle_hash, keccak256(concatenated));

    assert!(result.execution_time_us > 0, "execution_time_us should be greater than zero");

    Ok(())
}

#[test]
fn meter_bundle_multiple_transactions() -> eyre::Result<()> {
    let harness = setup_harness()?;

    let to_1 = Address::random();
    let to_2 = Address::random();

    // Create first transaction
    let signed_tx_1 = TransactionBuilder::default()
        .signer(harness.signer(Account::Alice))
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

    // Create second transaction
    let signed_tx_2 = TransactionBuilder::default()
        .signer(harness.signer(Account::Bob))
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

    let envelope_1 = envelope_from_signed(&tx_1)?;
    let envelope_2 = envelope_from_signed(&tx_2)?;
    let tx_hash_1 = envelope_1.tx_hash();
    let tx_hash_2 = envelope_2.tx_hash();

    let state_provider = harness
        .provider
        .state_by_block_hash(harness.header.hash())
        .context("getting state provider")?;

    let parsed_bundle = create_parsed_bundle(vec![envelope_1.clone(), envelope_2.clone()])?;

    let (results, total_gas_used, total_gas_fees, bundle_hash, total_execution_time) =
        meter_bundle(state_provider, harness.chain_spec.clone(), parsed_bundle, &harness.header)?;

    assert_eq!(results.len(), 2);
    assert!(total_execution_time > 0);

    // Check first transaction
    let result_1 = &results[0];
    assert_eq!(result_1.from_address, Account::Alice.address());
    assert_eq!(result_1.to_address, Some(to_1));
    assert_eq!(result_1.tx_hash, tx_hash_1);
    assert_eq!(result_1.gas_price, U256::from(10));
    assert_eq!(result_1.gas_used, 21_000);
    assert_eq!(result_1.coinbase_diff, (U256::from(21_000) * U256::from(10)),);

    // Check second transaction
    let result_2 = &results[1];
    assert_eq!(result_2.from_address, Account::Bob.address());
    assert_eq!(result_2.to_address, Some(to_2));
    assert_eq!(result_2.tx_hash, tx_hash_2);
    assert_eq!(result_2.gas_price, U256::from(15));
    assert_eq!(result_2.gas_used, 21_000);
    assert_eq!(result_2.coinbase_diff, U256::from(21_000) * U256::from(15),);

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

    assert!(result_1.execution_time_us > 0, "execution_time_us should be greater than zero");
    assert!(result_2.execution_time_us > 0, "execution_time_us should be greater than zero");

    Ok(())
}
