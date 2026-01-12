//! Integration tests for block metering functionality.

use alloy_consensus::{BlockHeader, Header};
use alloy_primitives::{Address, B256};
use base_client_node::test_utils::{Account, TestHarness, load_chain_spec};
use base_metering::{MeteringExtension, meter_block};
use reth::chainspec::EthChainSpec;
use reth_optimism_primitives::{OpBlock, OpBlockBody, OpTransactionSigned};
use reth_primitives_traits::Block as BlockT;
use reth_provider::HeaderProvider;
use reth_transaction_pool::test_utils::TransactionBuilder;

async fn setup() -> eyre::Result<TestHarness> {
    let chain_spec = load_chain_spec();
    TestHarness::builder()
        .with_chain_spec(chain_spec)
        .with_ext::<MeteringExtension>(true)
        .build()
        .await
}

fn create_block_with_transactions(
    harness: &TestHarness,
    transactions: Vec<OpTransactionSigned>,
) -> eyre::Result<OpBlock> {
    let provider = harness.blockchain_provider();
    let genesis = provider.sealed_header(0)?.expect("genesis header exists");

    let header = Header {
        parent_hash: genesis.hash(),
        number: genesis.number() + 1,
        timestamp: genesis.timestamp() + 2,
        gas_limit: 30_000_000,
        beneficiary: Address::random(),
        base_fee_per_gas: Some(1),
        // Required for post-Cancun blocks (EIP-4788)
        parent_beacon_block_root: Some(B256::ZERO),
        ..Default::default()
    };

    let body = OpBlockBody { transactions, ommers: vec![], withdrawals: None };

    Ok(OpBlock::new(header, body))
}

#[tokio::test]
async fn meter_block_empty_transactions() -> eyre::Result<()> {
    let harness = setup().await?;

    let block = create_block_with_transactions(&harness, vec![])?;

    let response = meter_block(harness.blockchain_provider(), harness.chain_spec(), &block)?;

    assert_eq!(response.block_hash, block.header().hash_slow());
    assert_eq!(response.block_number, block.header().number());
    assert!(response.transactions.is_empty());
    // No transactions means minimal signer recovery time (just timing overhead)
    assert!(response.execution_time_us > 0, "execution time should be non-zero due to EVM setup");
    assert!(response.state_root_time_us > 0, "state root time should be non-zero");
    assert_eq!(
        response.total_time_us,
        response.signer_recovery_time_us + response.execution_time_us + response.state_root_time_us
    );

    Ok(())
}

#[tokio::test]
async fn meter_block_single_transaction() -> eyre::Result<()> {
    let harness = setup().await?;
    let chain_spec = harness.chain_spec();

    let to = Address::random();
    let signed_tx = TransactionBuilder::default()
        .signer(Account::Alice.signer_b256())
        .chain_id(chain_spec.chain_id())
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

    let block = create_block_with_transactions(&harness, vec![tx])?;

    let response = meter_block(harness.blockchain_provider(), chain_spec, &block)?;

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

#[tokio::test]
async fn meter_block_multiple_transactions() -> eyre::Result<()> {
    let harness = setup().await?;
    let chain_spec = harness.chain_spec();

    let to_1 = Address::random();
    let to_2 = Address::random();

    // Create first transaction from Alice
    let signed_tx_1 = TransactionBuilder::default()
        .signer(Account::Alice.signer_b256())
        .chain_id(chain_spec.chain_id())
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
        .signer(Account::Bob.signer_b256())
        .chain_id(chain_spec.chain_id())
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

    let block = create_block_with_transactions(&harness, vec![tx_1, tx_2])?;

    let response = meter_block(harness.blockchain_provider(), chain_spec, &block)?;

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

#[tokio::test]
async fn meter_block_timing_consistency() -> eyre::Result<()> {
    let harness = setup().await?;
    let chain_spec = harness.chain_spec();

    // Create a block with one transaction
    let signed_tx = TransactionBuilder::default()
        .signer(Account::Alice.signer_b256())
        .chain_id(chain_spec.chain_id())
        .nonce(0)
        .to(Address::random())
        .value(1_000)
        .gas_limit(21_000)
        .max_fee_per_gas(10)
        .max_priority_fee_per_gas(1)
        .into_eip1559();

    let tx =
        OpTransactionSigned::Eip1559(signed_tx.as_eip1559().expect("eip1559 transaction").clone());

    let block = create_block_with_transactions(&harness, vec![tx])?;

    let response = meter_block(harness.blockchain_provider(), chain_spec, &block)?;

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

// ============================================================================
// Error Path Tests
// ============================================================================

#[tokio::test]
async fn meter_block_parent_header_not_found() -> eyre::Result<()> {
    let harness = setup().await?;
    let chain_spec = harness.chain_spec();
    let provider = harness.blockchain_provider();
    let genesis = provider.sealed_header(0)?.expect("genesis header exists");

    // Create a block that references a non-existent parent
    let fake_parent_hash = B256::random();
    let header = Header {
        parent_hash: fake_parent_hash, // This parent doesn't exist
        number: 999,
        timestamp: genesis.timestamp() + 2,
        gas_limit: 30_000_000,
        beneficiary: Address::random(),
        base_fee_per_gas: Some(1),
        parent_beacon_block_root: Some(B256::ZERO),
        ..Default::default()
    };

    let body = OpBlockBody { transactions: vec![], ommers: vec![], withdrawals: None };
    let block = OpBlock::new(header, body);

    let result = meter_block(provider, chain_spec, &block);

    assert!(result.is_err(), "should fail when parent header is not found");
    let err = result.unwrap_err();
    let err_str = err.to_string();
    assert!(
        err_str.contains("Parent header not found") || err_str.contains("not found"),
        "error should indicate parent header not found: {}",
        err_str
    );

    Ok(())
}

#[tokio::test]
async fn meter_block_invalid_transaction_signature() -> eyre::Result<()> {
    use alloy_consensus::TxEip1559;
    use alloy_primitives::Signature;

    let harness = setup().await?;
    let chain_spec = harness.chain_spec();

    // Create a transaction with an invalid signature
    let tx = TxEip1559 {
        chain_id: chain_spec.chain_id(),
        nonce: 0,
        gas_limit: 21_000,
        max_fee_per_gas: 10,
        max_priority_fee_per_gas: 1,
        to: alloy_primitives::TxKind::Call(Address::random()),
        value: alloy_primitives::U256::from(1000),
        access_list: Default::default(),
        input: Default::default(),
    };

    // Create a signature with invalid values (all zeros is invalid for secp256k1)
    let invalid_signature =
        Signature::new(alloy_primitives::U256::ZERO, alloy_primitives::U256::ZERO, false);

    let signed_tx = alloy_consensus::Signed::new_unchecked(tx, invalid_signature, B256::random());
    let op_tx = OpTransactionSigned::Eip1559(signed_tx);

    let block = create_block_with_transactions(&harness, vec![op_tx])?;

    let result = meter_block(harness.blockchain_provider(), chain_spec, &block);

    assert!(result.is_err(), "should fail when transaction has invalid signature");
    let err = result.unwrap_err();
    let err_str = err.to_string();
    assert!(
        err_str.contains("recover signer") || err_str.contains("signature"),
        "error should indicate signer recovery failure: {}",
        err_str
    );

    Ok(())
}
