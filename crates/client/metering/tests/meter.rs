//! Integration tests covering the Metering logic surface area.

use alloy_eips::Encodable2718;
use alloy_primitives::{Address, Bytes, U256, keccak256};
use base_bundles::{Bundle, ParsedBundle};
use base_reth_metering::{meter_bundle, test_utils::MeteringTestContext};
use base_reth_test_utils::{ALICE, BOB};
use eyre::Context;
use op_alloy_consensus::OpTxEnvelope;
use reth::chainspec::EthChainSpec;
use reth_optimism_primitives::OpTransactionSigned;
use reth_provider::StateProviderFactory;
use reth_transaction_pool::test_utils::TransactionBuilder;

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
    let ctx = MeteringTestContext::new()?;

    let state_provider = ctx
        .provider
        .state_by_block_hash(ctx.header.hash())
        .context("getting state provider")?;

    let parsed_bundle = create_parsed_bundle(Vec::new())?;

    let (results, total_gas_used, total_gas_fees, bundle_hash, total_execution_time) =
        meter_bundle(state_provider, ctx.chain_spec.clone(), parsed_bundle, &ctx.header)?;

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
    let ctx = MeteringTestContext::new()?;

    let to = Address::random();
    let signed_tx = TransactionBuilder::default()
        .signer(ALICE.signer_b256())
        .chain_id(ctx.chain_spec.chain_id())
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

    let state_provider = ctx
        .provider
        .state_by_block_hash(ctx.header.hash())
        .context("getting state provider")?;

    let parsed_bundle = create_parsed_bundle(vec![envelope.clone()])?;

    let (results, total_gas_used, total_gas_fees, bundle_hash, total_execution_time) =
        meter_bundle(state_provider, ctx.chain_spec.clone(), parsed_bundle, &ctx.header)?;

    assert_eq!(results.len(), 1);
    let result = &results[0];
    assert!(total_execution_time > 0);

    assert_eq!(result.from_address, ALICE.address);
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
    let ctx = MeteringTestContext::new()?;

    let to_1 = Address::random();
    let to_2 = Address::random();

    // Create first transaction
    let signed_tx_1 = TransactionBuilder::default()
        .signer(ALICE.signer_b256())
        .chain_id(ctx.chain_spec.chain_id())
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
        .signer(BOB.signer_b256())
        .chain_id(ctx.chain_spec.chain_id())
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

    let state_provider = ctx
        .provider
        .state_by_block_hash(ctx.header.hash())
        .context("getting state provider")?;

    let parsed_bundle = create_parsed_bundle(vec![envelope_1.clone(), envelope_2.clone()])?;

    let (results, total_gas_used, total_gas_fees, bundle_hash, total_execution_time) =
        meter_bundle(state_provider, ctx.chain_spec.clone(), parsed_bundle, &ctx.header)?;

    assert_eq!(results.len(), 2);
    assert!(total_execution_time > 0);

    // Check first transaction
    let result_1 = &results[0];
    assert_eq!(result_1.from_address, ALICE.address);
    assert_eq!(result_1.to_address, Some(to_1));
    assert_eq!(result_1.tx_hash, tx_hash_1);
    assert_eq!(result_1.gas_price, U256::from(10));
    assert_eq!(result_1.gas_used, 21_000);
    assert_eq!(result_1.coinbase_diff, (U256::from(21_000) * U256::from(10)),);

    // Check second transaction
    let result_2 = &results[1];
    assert_eq!(result_2.from_address, BOB.address);
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
