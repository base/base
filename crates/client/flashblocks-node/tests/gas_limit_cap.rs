//! Integration tests for EIP-7825 transaction gas limit cap enforcement.
//!
//! Base V1 introduces a per-transaction gas limit cap of 2^24 (16,777,216).
//! These tests verify that `eth_sendRawTransaction` correctly rejects
//! transactions exceeding this cap when V1 is active.

use std::sync::Arc;

use alloy_consensus::SignableTransaction;
use alloy_eips::eip2718::Encodable2718;
use alloy_network::TransactionBuilder;
use alloy_primitives::Bytes;
use alloy_provider::Provider;
use alloy_signer::SignerSync;
use base_common_rpc_types::OpTransactionRequest;
use base_execution_chainspec::BaseChainSpec;
use base_node_runner::test_utils::TestHarnessBuilder;
use base_test_utils::{Account, DEVNET_CHAIN_ID, build_test_genesis, build_test_genesis_v1};
use eyre::Result;

const GAS_LIMIT_CAP: u64 = 1 << 24; // 16,777,216

fn sign_tx_with_gas_limit(from: Account, to: alloy_primitives::Address, gas_limit: u64) -> Bytes {
    let tx_request = OpTransactionRequest::default()
        .from(from.address())
        .transaction_type(2u8)
        .with_gas_limit(gas_limit)
        .with_max_fee_per_gas(1_000_000_000)
        .with_max_priority_fee_per_gas(0)
        .with_chain_id(DEVNET_CHAIN_ID)
        .to(to)
        .with_nonce(0);

    let tx = tx_request.build_typed_tx().expect("valid transaction request");
    let signature = from.signer().sign_hash_sync(&tx.signature_hash()).expect("sign tx");
    let signed_tx = tx.into_signed(signature);
    signed_tx.encoded_2718().into()
}

#[tokio::test]
async fn v1_gas_limit_cap() -> Result<()> {
    let chain_spec = Arc::new(BaseChainSpec::from_genesis(build_test_genesis_v1()));
    let harness = TestHarnessBuilder::new().with_chain_spec(chain_spec).build().await?;

    // Reject tx above cap
    let raw_tx = sign_tx_with_gas_limit(Account::Alice, Account::Bob.address(), GAS_LIMIT_CAP + 1);
    let result = harness.provider().send_raw_transaction(&raw_tx).await;
    assert!(result.is_err(), "tx with gas_limit > cap should be rejected when V1 is active");

    // Accept tx within cap
    let raw_tx = sign_tx_with_gas_limit(Account::Alice, Account::Bob.address(), 21_000);
    let result = harness.provider().send_raw_transaction(&raw_tx).await;
    assert!(result.is_ok(), "tx with gas_limit <= cap should be accepted when V1 is active");

    Ok(())
}

#[tokio::test]
async fn pre_v1_accepts_tx_above_gas_limit_cap() -> Result<()> {
    let chain_spec = Arc::new(BaseChainSpec::from_genesis(build_test_genesis()));
    let harness = TestHarnessBuilder::new().with_chain_spec(chain_spec).build().await?;

    let raw_tx = sign_tx_with_gas_limit(Account::Alice, Account::Bob.address(), GAS_LIMIT_CAP + 1);

    let result = harness.provider().send_raw_transaction(&raw_tx).await;
    assert!(result.is_ok(), "tx with gas_limit > cap should be accepted when V1 is not active");

    Ok(())
}
