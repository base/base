//! Bundle metering logic.

use std::{sync::Arc, time::Instant};

use alloy_consensus::{BlockHeader, Transaction as _, transaction::SignerRecoverable};
use alloy_primitives::{B256, U256};
use base_bundles::{BundleExtensions, BundleTxs, ParsedBundle, TransactionResult};
use eyre::{Result as EyreResult, eyre};
use reth::revm::db::{BundleState, Cache, CacheDB, State};
use reth_evm::{ConfigureEvm, execute::BlockBuilder};
use reth_optimism_chainspec::OpChainSpec;
use reth_optimism_evm::{OpEvmConfig, OpNextBlockEnvAttributes};
use reth_primitives_traits::SealedHeader;
use revm_database::states::bundle_state::BundleRetention;

/// State from pending flashblocks that is used as a base for metering
#[derive(Debug, Clone)]
pub struct FlashblocksState {
    /// The cache of account and storage data
    pub cache: Cache,
    /// The accumulated bundle of state changes
    pub bundle_state: BundleState,
}

const BLOCK_TIME: u64 = 2; // 2 seconds per block

/// Output from metering a bundle of transactions
#[derive(Debug)]
pub struct MeterBundleOutput {
    /// Transaction results with individual metrics
    pub results: Vec<TransactionResult>,
    /// Total gas used by all transactions
    pub total_gas_used: u64,
    /// Total gas fees paid by all transactions
    pub total_gas_fees: U256,
    /// Bundle hash
    pub bundle_hash: B256,
    /// Total time in microseconds (includes transaction execution and state root calculation)
    pub total_time_us: u128,
    /// State root calculation time in microseconds
    pub state_root_time_us: u128,
}

/// Simulates and meters a bundle of transactions
///
/// Takes a state provider, chain spec, parsed bundle, block header, and optional flashblocks state,
/// then executes transactions in sequence to measure gas usage and execution time.
///
/// Returns [`MeterBundleOutput`] containing transaction results and aggregated metrics.
pub fn meter_bundle<SP>(
    state_provider: SP,
    chain_spec: Arc<OpChainSpec>,
    bundle: ParsedBundle,
    header: &SealedHeader,
    flashblocks_state: Option<FlashblocksState>,
) -> EyreResult<MeterBundleOutput>
where
    SP: reth_provider::StateProvider,
{
    // Get bundle hash
    let bundle_hash = bundle.bundle_hash();

    // Create state database
    let state_db = reth::revm::database::StateProviderDatabase::new(state_provider);

    // Apply flashblocks read cache if available
    let cache_db = if let Some(ref flashblocks) = flashblocks_state {
        CacheDB { cache: flashblocks.cache.clone(), db: state_db }
    } else {
        CacheDB::new(state_db)
    };

    // Track bundle state changes. If metering using flashblocks state, include its bundle prestate.
    let mut db = if let Some(flashblocks) = flashblocks_state.as_ref() {
        State::builder()
            .with_database(cache_db)
            .with_bundle_update()
            .with_bundle_prestate(flashblocks.bundle_state.clone())
            .build()
    } else {
        State::builder().with_database(cache_db).with_bundle_update().build()
    };

    // Set up next block attributes
    // Use bundle.min_timestamp if provided, otherwise use header timestamp + BLOCK_TIME
    let timestamp = bundle.min_timestamp.unwrap_or_else(|| header.timestamp() + BLOCK_TIME);
    let attributes = OpNextBlockEnvAttributes {
        timestamp,
        suggested_fee_recipient: header.beneficiary(),
        prev_randao: header.mix_hash().unwrap_or(B256::random()),
        gas_limit: header.gas_limit(),
        parent_beacon_block_root: header.parent_beacon_block_root(),
        extra_data: header.extra_data().clone(),
    };

    // Execute transactions
    let mut results = Vec::new();
    let mut total_gas_used = 0u64;
    let mut total_gas_fees = U256::ZERO;

    let total_start = Instant::now();
    {
        let evm_config = OpEvmConfig::optimism(chain_spec);
        let mut builder = evm_config.builder_for_next_block(&mut db, header, attributes)?;

        builder.apply_pre_execution_changes()?;

        for tx in bundle.transactions() {
            let tx_start = Instant::now();
            let tx_hash = tx.tx_hash();
            let from = tx.recover_signer()?;
            let to = tx.to();
            let value = tx.value();
            let gas_price = tx.max_fee_per_gas();

            let gas_used = builder
                .execute_transaction(tx.clone())
                .map_err(|e| eyre!("Transaction {} execution failed: {}", tx_hash, e))?;

            let gas_fees = U256::from(gas_used) * U256::from(gas_price);
            total_gas_used = total_gas_used.saturating_add(gas_used);
            total_gas_fees = total_gas_fees.saturating_add(gas_fees);

            let execution_time = tx_start.elapsed().as_micros();

            results.push(TransactionResult {
                coinbase_diff: gas_fees,
                eth_sent_to_coinbase: U256::ZERO,
                from_address: from,
                gas_fees,
                gas_price: U256::from(gas_price),
                gas_used,
                to_address: to,
                tx_hash,
                value,
                execution_time_us: execution_time,
            });
        }
    }

    // Calculate state root and measure its calculation time. The bundle already includes
    // flashblocks state if it was provided via with_bundle_prestate.
    db.merge_transitions(BundleRetention::Reverts);
    let bundle_update = db.take_bundle();
    let state_provider = db.database.db.as_ref();

    let state_root_start = Instant::now();
    let hashed_state = state_provider.hashed_post_state(&bundle_update);
    let _ = state_provider.state_root_with_updates(hashed_state)?;
    let state_root_time_us = state_root_start.elapsed().as_micros();
    let total_time_us = total_start.elapsed().as_micros();

    Ok(MeterBundleOutput {
        results,
        total_gas_used,
        total_gas_fees,
        bundle_hash,
        total_time_us,
        state_root_time_us,
    })
}

#[cfg(test)]
mod tests {
    use alloy_eips::Encodable2718;
    use alloy_primitives::{Address, Bytes, keccak256};
    use base_bundles::{Bundle, ParsedBundle};
    use base_reth_test_utils::{ALICE, BOB};
    use eyre::Context;
    use op_alloy_consensus::OpTxEnvelope;
    use reth::{chainspec::EthChainSpec, revm::primitives::KECCAK_EMPTY};
    use reth_optimism_primitives::OpTransactionSigned;
    use reth_provider::StateProviderFactory;
    use reth_transaction_pool::test_utils::TransactionBuilder;

    use super::*;
    use crate::test_utils::MeteringTestContext;

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

        let output =
            meter_bundle(state_provider, ctx.chain_spec.clone(), parsed_bundle, &ctx.header, None)?;

        assert!(output.results.is_empty());
        assert_eq!(output.total_gas_used, 0);
        assert_eq!(output.total_gas_fees, U256::ZERO);
        // Even empty bundles have some EVM setup overhead
        assert!(output.total_time_us > 0);
        assert!(output.state_root_time_us > 0);
        assert_eq!(output.bundle_hash, keccak256([]));

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

        let tx = OpTransactionSigned::Eip1559(
            signed_tx.as_eip1559().expect("eip1559 transaction").clone(),
        );

        let envelope = envelope_from_signed(&tx)?;
        let tx_hash = envelope.tx_hash();

        let state_provider = ctx
            .provider
            .state_by_block_hash(ctx.header.hash())
            .context("getting state provider")?;

        let parsed_bundle = create_parsed_bundle(vec![envelope.clone()])?;

        let output =
            meter_bundle(state_provider, ctx.chain_spec.clone(), parsed_bundle, &ctx.header, None)?;

        assert_eq!(output.results.len(), 1);
        let result = &output.results[0];
        assert!(output.total_time_us > 0);
        assert!(output.state_root_time_us > 0);

        assert_eq!(result.from_address, ALICE.address);
        assert_eq!(result.to_address, Some(to));
        assert_eq!(result.tx_hash, tx_hash);
        assert_eq!(result.gas_price, U256::from(10));
        assert_eq!(result.gas_used, 21_000);
        assert_eq!(result.coinbase_diff, (U256::from(21_000) * U256::from(10)),);

        assert_eq!(output.total_gas_used, 21_000);
        assert_eq!(output.total_gas_fees, U256::from(21_000) * U256::from(10));

        let mut concatenated = Vec::with_capacity(32);
        concatenated.extend_from_slice(tx_hash.as_slice());
        assert_eq!(output.bundle_hash, keccak256(concatenated));

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

        let output =
            meter_bundle(state_provider, ctx.chain_spec.clone(), parsed_bundle, &ctx.header, None)?;

        assert_eq!(output.results.len(), 2);
        assert!(output.total_time_us > 0);
        assert!(output.state_root_time_us > 0);

        // Check first transaction
        let result_1 = &output.results[0];
        assert_eq!(result_1.from_address, ALICE.address);
        assert_eq!(result_1.to_address, Some(to_1));
        assert_eq!(result_1.tx_hash, tx_hash_1);
        assert_eq!(result_1.gas_price, U256::from(10));
        assert_eq!(result_1.gas_used, 21_000);
        assert_eq!(result_1.coinbase_diff, (U256::from(21_000) * U256::from(10)),);

        // Check second transaction
        let result_2 = &output.results[1];
        assert_eq!(result_2.from_address, BOB.address);
        assert_eq!(result_2.to_address, Some(to_2));
        assert_eq!(result_2.tx_hash, tx_hash_2);
        assert_eq!(result_2.gas_price, U256::from(15));
        assert_eq!(result_2.gas_used, 21_000);
        assert_eq!(result_2.coinbase_diff, U256::from(21_000) * U256::from(15),);

        // Check aggregated values
        assert_eq!(output.total_gas_used, 42_000);
        let expected_total_fees =
            U256::from(21_000) * U256::from(10) + U256::from(21_000) * U256::from(15);
        assert_eq!(output.total_gas_fees, expected_total_fees);

        // Check bundle hash includes both transactions
        let mut concatenated = Vec::with_capacity(64);
        concatenated.extend_from_slice(tx_hash_1.as_slice());
        concatenated.extend_from_slice(tx_hash_2.as_slice());
        assert_eq!(output.bundle_hash, keccak256(concatenated));

        assert!(result_1.execution_time_us > 0, "execution_time_us should be greater than zero");
        assert!(result_2.execution_time_us > 0, "execution_time_us should be greater than zero");

        Ok(())
    }

    #[test]
    fn meter_bundle_state_root_time_invariant() -> eyre::Result<()> {
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

        let tx = OpTransactionSigned::Eip1559(
            signed_tx.as_eip1559().expect("eip1559 transaction").clone(),
        );

        let envelope = envelope_from_signed(&tx)?;

        let state_provider = ctx
            .provider
            .state_by_block_hash(ctx.header.hash())
            .context("getting state provider")?;

        let parsed_bundle = create_parsed_bundle(vec![envelope.clone()])?;

        let output =
            meter_bundle(state_provider, ctx.chain_spec.clone(), parsed_bundle, &ctx.header, None)?;

        // Verify invariant: total time must include state root time
        assert!(
            output.total_time_us >= output.state_root_time_us,
            "total_time_us ({}) should be >= state_root_time_us ({})",
            output.total_time_us,
            output.state_root_time_us
        );

        // State root time should be non-zero
        assert!(output.state_root_time_us > 0, "state_root_time_us should be greater than zero");

        Ok(())
    }

    /// Integration test: verifies meter_bundle uses flashblocks state correctly.
    ///
    /// A transaction using nonce=1 should fail without flashblocks state (since
    /// canonical nonce is 0), but succeed when flashblocks state indicates nonce=1.
    #[test]
    fn meter_bundle_requires_correct_layering_for_pending_nonce() -> eyre::Result<()> {
        let ctx = MeteringTestContext::new()?;

        // Create a transaction that requires nonce=1 (assuming canonical nonce is 0)
        let to = Address::random();
        let signed_tx = TransactionBuilder::default()
            .signer(ALICE.signer_b256())
            .chain_id(ctx.chain_spec.chain_id())
            .nonce(1) // Requires pending state to have nonce=1
            .to(to)
            .value(100)
            .gas_limit(21_000)
            .max_fee_per_gas(10)
            .max_priority_fee_per_gas(1)
            .into_eip1559();

        let tx = OpTransactionSigned::Eip1559(
            signed_tx.as_eip1559().expect("eip1559 transaction").clone(),
        );
        let envelope = envelope_from_signed(&tx)?;
        let parsed_bundle = create_parsed_bundle(vec![envelope])?;

        // Without flashblocks state, transaction should fail (nonce mismatch)
        let state_provider = ctx
            .provider
            .state_by_block_hash(ctx.header.hash())
            .context("getting state provider")?;

        let result_without_flashblocks = meter_bundle(
            state_provider,
            ctx.chain_spec.clone(),
            parsed_bundle.clone(),
            &ctx.header,
            None, // No flashblocks state
        );

        assert!(
            result_without_flashblocks.is_err(),
            "Transaction with nonce=1 should fail without pending state (canonical nonce is 0)"
        );

        // Now create flashblocks state with nonce=1 for Alice
        // Use BundleState::new() to properly calculate state_size
        let bundle_state =
            BundleState::new(
                [(
                    ALICE.address,
                    Some(reth::revm::state::AccountInfo {
                        balance: U256::from(1_000_000_000u64),
                        nonce: 0, // original
                        code_hash: KECCAK_EMPTY,
                        code: None,
                    }),
                    Some(reth::revm::state::AccountInfo {
                        balance: U256::from(1_000_000_000u64),
                        nonce: 1, // pending (after first flashblock tx)
                        code_hash: KECCAK_EMPTY,
                        code: None,
                    }),
                    Default::default(), // no storage changes
                )],
                Vec::<
                    Vec<(
                        Address,
                        Option<Option<reth::revm::state::AccountInfo>>,
                        Vec<(U256, U256)>,
                    )>,
                >::new(),
                Vec::<(B256, reth::revm::bytecode::Bytecode)>::new(),
            );

        let flashblocks_state = FlashblocksState { cache: Cache::default(), bundle_state };

        // With correct flashblocks state, transaction should succeed
        let state_provider2 = ctx
            .provider
            .state_by_block_hash(ctx.header.hash())
            .context("getting state provider")?;

        let result_with_flashblocks = meter_bundle(
            state_provider2,
            ctx.chain_spec.clone(),
            parsed_bundle,
            &ctx.header,
            Some(flashblocks_state),
        );

        assert!(
            result_with_flashblocks.is_ok(),
            "Transaction with nonce=1 should succeed with pending state showing nonce=1: {:?}",
            result_with_flashblocks.err()
        );

        Ok(())
    }
}
