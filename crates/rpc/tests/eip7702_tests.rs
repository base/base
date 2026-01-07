//! EIP-7702 delegation transaction tests for pending state.
//!
//! These tests verify that EIP-7702 authorization and delegation
//! transactions work correctly in the pending/flashblocks state.

use alloy_consensus::{SignableTransaction, TxEip1559, TxEip7702};
use alloy_eips::{eip2718::Encodable2718, eip7702::Authorization};
use alloy_primitives::{Address, B256, Bytes, U256};
use alloy_provider::Provider;
use alloy_sol_types::SolCall;
use base_flashtypes::{
    ExecutionPayloadBaseV1, ExecutionPayloadFlashblockDeltaV1, Flashblock, Metadata,
};
use base_reth_test_utils::{
    Account, FlashblocksHarness, L1_BLOCK_INFO_DEPOSIT_TX, Minimal7702Account, SignerSync,
};
use eyre::Result;
use op_alloy_network::ReceiptResponse;

/// Cumulative gas used after the base flashblock (deposit tx + contract deployment)
/// This value must be used as the starting point for subsequent flashblocks.
const BASE_CUMULATIVE_GAS: u64 = 500000;

/// Test setup that holds harness and deployed contract info
struct TestSetup {
    harness: FlashblocksHarness,
    account_contract_address: Address,
    account_deploy_tx: Bytes,
}

impl TestSetup {
    async fn new() -> Result<Self> {
        let harness = FlashblocksHarness::new().await?;
        let deployer = &harness.accounts().deployer;

        // Deploy Minimal7702Account contract
        let deploy_data = Minimal7702Account::BYTECODE.to_vec();
        let (account_deploy_tx, account_contract_address, _) =
            deployer.create_deployment_tx(Bytes::from(deploy_data), 0)?;

        Ok(Self { harness, account_contract_address, account_deploy_tx })
    }

    async fn send_flashblock(&self, flashblock: Flashblock) -> Result<()> {
        self.harness.send_flashblock(flashblock).await
    }

    fn provider(&self) -> alloy_provider::RootProvider<op_alloy_network::Optimism> {
        self.harness.provider()
    }

    fn chain_id(&self) -> u64 {
        84532 // Base Sepolia chain ID (matches test harness)
    }

    fn alice(&self) -> &Account {
        &self.harness.accounts().alice
    }

    fn bob(&self) -> &Account {
        &self.harness.accounts().bob
    }
}

/// Build an EIP-7702 authorization for delegating to a contract
fn build_authorization(
    chain_id: u64,
    contract_address: Address,
    nonce: u64,
    account: &Account,
) -> alloy_eips::eip7702::SignedAuthorization {
    let auth = Authorization { chain_id: U256::from(chain_id), address: contract_address, nonce };

    let signature = account.signer().sign_hash_sync(&auth.signature_hash()).expect("signing works");
    auth.into_signed(signature)
}

/// Build and sign an EIP-7702 transaction
fn build_eip7702_tx(
    chain_id: u64,
    nonce: u64,
    to: Address,
    value: U256,
    input: Bytes,
    authorization_list: Vec<alloy_eips::eip7702::SignedAuthorization>,
    account: &Account,
) -> Bytes {
    let tx = TxEip7702 {
        chain_id,
        nonce,
        gas_limit: 200_000,
        max_fee_per_gas: 1_000_000_000,
        max_priority_fee_per_gas: 1_000_000_000,
        to,
        value,
        access_list: Default::default(),
        authorization_list,
        input,
    };

    let signature = account.signer().sign_hash_sync(&tx.signature_hash()).expect("signing works");
    let signed = tx.into_signed(signature);

    signed.encoded_2718().into()
}

/// Build and sign an EIP-1559 transaction
fn build_eip1559_tx(
    chain_id: u64,
    nonce: u64,
    to: Address,
    value: U256,
    input: Bytes,
    account: &Account,
) -> Bytes {
    let tx = TxEip1559 {
        chain_id,
        nonce,
        gas_limit: 200_000,
        max_fee_per_gas: 1_000_000_000,
        max_priority_fee_per_gas: 1_000_000_000,
        to: alloy_primitives::TxKind::Call(to),
        value,
        access_list: Default::default(),
        input,
    };

    let signature = account.signer().sign_hash_sync(&tx.signature_hash()).expect("signing works");
    let signed = tx.into_signed(signature);

    signed.encoded_2718().into()
}

fn create_base_flashblock(setup: &TestSetup) -> Flashblock {
    Flashblock {
        payload_id: alloy_rpc_types_engine::PayloadId::new([0; 8]),
        index: 0,
        base: Some(ExecutionPayloadBaseV1 {
            parent_beacon_block_root: B256::default(),
            parent_hash: B256::default(),
            fee_recipient: Address::ZERO,
            prev_randao: B256::default(),
            block_number: 1,
            gas_limit: 30_000_000,
            timestamp: 0,
            extra_data: Bytes::new(),
            base_fee_per_gas: U256::ZERO,
        }),
        diff: ExecutionPayloadFlashblockDeltaV1 {
            blob_gas_used: Some(0),
            transactions: vec![L1_BLOCK_INFO_DEPOSIT_TX.clone(), setup.account_deploy_tx.clone()],
            ..Default::default()
        },
        metadata: Metadata { block_number: 1 },
    }
}

fn create_eip7702_flashblock(eip7702_tx: Bytes, cumulative_gas: u64) -> Flashblock {
    Flashblock {
        payload_id: alloy_rpc_types_engine::PayloadId::new([0; 8]),
        index: 1,
        base: None,
        diff: ExecutionPayloadFlashblockDeltaV1 {
            state_root: B256::default(),
            receipts_root: B256::default(),
            gas_used: cumulative_gas,
            block_hash: B256::default(),
            blob_gas_used: Some(0),
            transactions: vec![eip7702_tx],
            withdrawals: Vec::new(),
            logs_bloom: Default::default(),
            withdrawals_root: Default::default(),
        },
        metadata: Metadata { block_number: 1 },
    }
}

/// Test that an EIP-7702 delegation transaction can be included in a flashblock
/// and the pending state reflects the delegation.
#[tokio::test]
async fn test_eip7702_delegation_in_pending_flashblock() -> Result<()> {
    let setup = TestSetup::new().await?;
    let chain_id = setup.chain_id();

    // Send base flashblock with contract deployment
    let base_payload = create_base_flashblock(&setup);
    setup.send_flashblock(base_payload).await?;

    // Create authorization for Alice to delegate to the Minimal7702Account contract
    let auth = build_authorization(chain_id, setup.account_contract_address, 0, setup.alice());

    // Build EIP-7702 transaction with authorization
    // This delegates Alice's EOA to execute code from Minimal7702Account
    let increment_call = Minimal7702Account::incrementCall {};
    let eip7702_tx = build_eip7702_tx(
        chain_id,
        0,
        setup.alice().address,
        U256::ZERO,
        Bytes::from(increment_call.abi_encode()),
        vec![auth],
        setup.alice(),
    );

    let tx_hash = alloy_primitives::keccak256(&eip7702_tx);

    // Create flashblock with the EIP-7702 transaction
    // Cumulative gas must continue from where the base flashblock left off
    let eip7702_flashblock = create_eip7702_flashblock(eip7702_tx, BASE_CUMULATIVE_GAS + 50000);
    setup.send_flashblock(eip7702_flashblock).await?;

    // Query pending transaction to verify it was included
    let provider = setup.provider();
    let pending_tx = provider.get_transaction_by_hash(tx_hash).await?;

    assert!(pending_tx.is_some(), "EIP-7702 transaction should be in pending state");

    Ok(())
}

/// Test that multiple EIP-7702 delegations in the same flashblock work correctly
#[tokio::test]
async fn test_eip7702_multiple_delegations_same_flashblock() -> Result<()> {
    let setup = TestSetup::new().await?;
    let chain_id = setup.chain_id();

    // Send base flashblock with contract deployment
    let base_payload = create_base_flashblock(&setup);
    setup.send_flashblock(base_payload).await?;

    // Create authorizations for both Alice and Bob
    let auth_alice =
        build_authorization(chain_id, setup.account_contract_address, 0, setup.alice());
    let auth_bob = build_authorization(chain_id, setup.account_contract_address, 0, setup.bob());

    // Build EIP-7702 transactions
    let increment_call = Minimal7702Account::incrementCall {};
    let tx_alice = build_eip7702_tx(
        chain_id,
        0,
        setup.alice().address,
        U256::ZERO,
        Bytes::from(increment_call.abi_encode()),
        vec![auth_alice],
        setup.alice(),
    );

    let tx_bob = build_eip7702_tx(
        chain_id,
        0,
        setup.bob().address,
        U256::ZERO,
        Bytes::from(increment_call.abi_encode()),
        vec![auth_bob],
        setup.bob(),
    );

    let tx_hash_alice = alloy_primitives::keccak256(&tx_alice);
    let tx_hash_bob = alloy_primitives::keccak256(&tx_bob);

    // Create flashblock with both transactions
    // Cumulative gas must continue from where the base flashblock left off
    let bob_cumulative = BASE_CUMULATIVE_GAS + 100000;
    let flashblock = Flashblock {
        payload_id: alloy_rpc_types_engine::PayloadId::new([0; 8]),
        index: 1,
        base: None,
        diff: ExecutionPayloadFlashblockDeltaV1 {
            state_root: B256::default(),
            receipts_root: B256::default(),
            gas_used: bob_cumulative,
            block_hash: B256::default(),
            blob_gas_used: Some(0),
            transactions: vec![tx_alice, tx_bob],
            withdrawals: Vec::new(),
            logs_bloom: Default::default(),
            withdrawals_root: Default::default(),
        },
        metadata: Metadata { block_number: 1 },
    };

    setup.send_flashblock(flashblock).await?;

    // Verify both transactions are in pending state
    let provider = setup.provider();
    let pending_alice = provider.get_transaction_by_hash(tx_hash_alice).await?;
    let pending_bob = provider.get_transaction_by_hash(tx_hash_bob).await?;

    assert!(pending_alice.is_some(), "Alice's EIP-7702 tx should be in pending state");
    assert!(pending_bob.is_some(), "Bob's EIP-7702 tx should be in pending state");

    Ok(())
}

/// Test that EIP-7702 transaction receipts are correctly returned from pending state
#[tokio::test]
async fn test_eip7702_pending_receipt() -> Result<()> {
    let setup = TestSetup::new().await?;
    let chain_id = setup.chain_id();

    // Send base flashblock with contract deployment
    let base_payload = create_base_flashblock(&setup);
    setup.send_flashblock(base_payload).await?;

    // Create and send EIP-7702 transaction
    let auth = build_authorization(chain_id, setup.account_contract_address, 0, setup.alice());
    let increment_call = Minimal7702Account::incrementCall {};
    let eip7702_tx = build_eip7702_tx(
        chain_id,
        0,
        setup.alice().address,
        U256::ZERO,
        Bytes::from(increment_call.abi_encode()),
        vec![auth],
        setup.alice(),
    );

    let tx_hash = alloy_primitives::keccak256(&eip7702_tx);
    // Cumulative gas must continue from where the base flashblock left off
    let eip7702_flashblock = create_eip7702_flashblock(eip7702_tx, BASE_CUMULATIVE_GAS + 50000);
    setup.send_flashblock(eip7702_flashblock).await?;

    // Query receipt from pending state
    let provider = setup.provider();
    let receipt = provider.get_transaction_receipt(tx_hash).await?;

    assert!(receipt.is_some(), "EIP-7702 receipt should be available in pending state");
    let receipt = receipt.unwrap();
    assert!(receipt.status(), "EIP-7702 transaction should have succeeded");

    Ok(())
}

/// Test EIP-7702 delegation followed by execution in subsequent flashblock.
#[tokio::test]
async fn test_eip7702_delegation_then_execution() -> Result<()> {
    let setup = TestSetup::new().await?;
    let chain_id = setup.chain_id();

    // Send base flashblock with contract deployment
    let base_payload = create_base_flashblock(&setup);
    setup.send_flashblock(base_payload).await?;

    // First flashblock: delegation only (no execution)
    // Cumulative gas continues from base flashblock (500000)
    let delegation_gas = 30000;
    let delegation_cumulative = BASE_CUMULATIVE_GAS + delegation_gas;

    let auth = build_authorization(chain_id, setup.account_contract_address, 0, setup.alice());
    let delegation_tx = build_eip7702_tx(
        chain_id,
        0,
        setup.alice().address,
        U256::ZERO,
        Bytes::new(), // Empty input - just setting up delegation
        vec![auth],
        setup.alice(),
    );

    let delegation_hash = alloy_primitives::keccak256(&delegation_tx);
    let delegation_flashblock = create_eip7702_flashblock(delegation_tx, delegation_cumulative);
    setup.send_flashblock(delegation_flashblock).await?;

    // Second flashblock: execute through delegated account
    // After delegation, calls to Alice's address execute Minimal7702Account code
    // Use EIP-1559 transaction since the delegation is already set up
    // Cumulative gas continues from delegation flashblock
    let execution_gas = 25000;
    let execution_cumulative = delegation_cumulative + execution_gas;

    let increment_call = Minimal7702Account::incrementCall {};
    let execution_tx = build_eip1559_tx(
        chain_id,
        1, // incremented nonce
        setup.alice().address,
        U256::ZERO,
        Bytes::from(increment_call.abi_encode()),
        setup.alice(),
    );

    let execution_hash = alloy_primitives::keccak256(&execution_tx);
    let execution_flashblock = Flashblock {
        payload_id: alloy_rpc_types_engine::PayloadId::new([0; 8]),
        index: 2,
        base: None,
        diff: ExecutionPayloadFlashblockDeltaV1 {
            state_root: B256::default(),
            receipts_root: B256::default(),
            gas_used: execution_cumulative,
            block_hash: B256::default(),
            blob_gas_used: Some(0),
            transactions: vec![execution_tx],
            withdrawals: Vec::new(),
            logs_bloom: Default::default(),
            withdrawals_root: Default::default(),
        },
        metadata: Metadata { block_number: 1 },
    };

    setup.send_flashblock(execution_flashblock).await?;

    // Verify both transactions are in pending state
    let provider = setup.provider();
    let delegation_receipt = provider.get_transaction_receipt(delegation_hash).await?;
    let execution_receipt = provider.get_transaction_receipt(execution_hash).await?;

    assert!(delegation_receipt.is_some(), "Delegation tx receipt should exist");
    assert!(execution_receipt.is_some(), "Execution tx receipt should exist");

    Ok(())
}
