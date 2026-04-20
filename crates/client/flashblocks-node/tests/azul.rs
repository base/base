//! Integration tests for EIP-7825 transaction gas limit cap enforcement and
//! omitted-gas RPC behavior on Azul.
//!
//! Base Azul introduces a per-transaction gas limit cap of 2^24 (16,777,216).
//! These tests verify that:
//! - `eth_sendRawTransaction` correctly rejects transactions exceeding this cap
//!   when Azul is active.
//! - RPC methods that estimate or fill gas continue to work when `gas` is
//!   omitted, both with and without calldata.

use std::sync::Arc;

use alloy_consensus::{SignableTransaction, Transaction};
use alloy_eips::{BlockNumberOrTag, eip2718::Encodable2718};
use alloy_network::TransactionBuilder;
use alloy_primitives::{Bytes, address, bytes};
use alloy_provider::Provider;
use alloy_rpc_types_eth::TransactionInput;
use alloy_signer::SignerSync;
use base_common_rpc_types::BaseTransactionRequest;
use base_execution_chainspec::BaseChainSpec;
use base_node_runner::test_utils::TestHarnessBuilder;
use base_test_utils::{Account, DEVNET_CHAIN_ID, build_test_genesis, build_test_genesis_v1};
use eyre::Result;

const GAS_LIMIT_CAP: u64 = 1 << 24; // 16,777,216

fn sign_tx_with_gas_limit(from: Account, to: alloy_primitives::Address, gas_limit: u64) -> Bytes {
    let tx_request = BaseTransactionRequest::default()
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

fn transfer_request_without_gas() -> BaseTransactionRequest {
    BaseTransactionRequest::default().from(Account::Alice.address()).to(Account::Bob.address())
}

fn transfer_request_with_data_without_gas() -> BaseTransactionRequest {
    transfer_request_without_gas().input(TransactionInput::new(Bytes::from_static(&[0x00])))
}

fn transfer_request_with_gas_and_data() -> BaseTransactionRequest {
    transfer_request_with_data_without_gas().gas_limit(100_000)
}

#[tokio::test]
async fn azul_gas_limit_cap() -> Result<()> {
    let chain_spec = Arc::new(BaseChainSpec::from_genesis(build_test_genesis_v1()));
    let harness = TestHarnessBuilder::new().with_chain_spec(chain_spec).build().await?;

    // Reject tx above cap
    let raw_tx = sign_tx_with_gas_limit(Account::Alice, Account::Bob.address(), GAS_LIMIT_CAP + 1);
    let result = harness.provider().send_raw_transaction(&raw_tx).await;
    assert!(result.is_err(), "tx with gas_limit > cap should be rejected when Azul is active");

    // Accept tx within cap
    let raw_tx = sign_tx_with_gas_limit(Account::Alice, Account::Bob.address(), 21_000);
    let result = harness.provider().send_raw_transaction(&raw_tx).await;
    assert!(result.is_ok(), "tx with gas_limit <= cap should be accepted when Azul is active");

    Ok(())
}

#[tokio::test]
async fn pre_azul_accepts_tx_above_gas_limit_cap() -> Result<()> {
    let chain_spec = Arc::new(BaseChainSpec::from_genesis(build_test_genesis()));
    let harness = TestHarnessBuilder::new().with_chain_spec(chain_spec).build().await?;

    let raw_tx = sign_tx_with_gas_limit(Account::Alice, Account::Bob.address(), GAS_LIMIT_CAP + 1);

    let result = harness.provider().send_raw_transaction(&raw_tx).await;
    assert!(result.is_ok(), "tx with gas_limit > cap should be accepted when Azul is not active");

    Ok(())
}

#[tokio::test]
async fn azul_estimate_gas_without_data_returns_transfer_gas() -> Result<()> {
    let chain_spec = Arc::new(BaseChainSpec::from_genesis(build_test_genesis_v1()));
    let harness = TestHarnessBuilder::new().with_chain_spec(chain_spec).build().await?;
    let gas = harness.provider().estimate_gas(transfer_request_without_gas()).await?;
    assert_eq!(gas, 21_000);

    Ok(())
}

#[tokio::test]
async fn azul_estimate_gas_with_data_accepts_implicit_gas_limit() -> Result<()> {
    let chain_spec = Arc::new(BaseChainSpec::from_genesis(build_test_genesis_v1()));
    let harness = TestHarnessBuilder::new().with_chain_spec(chain_spec).build().await?;
    let gas = harness.provider().estimate_gas(transfer_request_with_data_without_gas()).await?;
    assert!(gas > 21_000, "tx with calldata should exceed plain transfer gas");

    Ok(())
}

#[tokio::test]
async fn azul_estimate_gas_with_explicit_gas_and_data_passes() -> Result<()> {
    let chain_spec = Arc::new(BaseChainSpec::from_genesis(build_test_genesis_v1()));
    let harness = TestHarnessBuilder::new().with_chain_spec(chain_spec).build().await?;
    let gas = harness.provider().estimate_gas(transfer_request_with_gas_and_data()).await?;
    assert!(gas > 21_000, "tx with calldata should exceed plain transfer gas");
    assert!(gas <= 100_000, "estimate should respect the provided gas cap");

    Ok(())
}

#[tokio::test]
async fn azul_eth_call_without_data_accepts_implicit_gas_limit() -> Result<()> {
    let chain_spec = Arc::new(BaseChainSpec::from_genesis(build_test_genesis_v1()));
    let harness = TestHarnessBuilder::new().with_chain_spec(chain_spec).build().await?;
    let result = harness
        .provider()
        .call(transfer_request_without_gas())
        .block(BlockNumberOrTag::Latest.into())
        .await?;
    assert!(result.is_empty(), "plain eth_call to EOA should return empty bytes");

    Ok(())
}

#[tokio::test]
async fn azul_eth_call_with_data_to_contract_accepts_implicit_gas_limit() -> Result<()> {
    let chain_spec = Arc::new(BaseChainSpec::from_genesis(build_test_genesis_v1()));
    let harness = TestHarnessBuilder::new().with_chain_spec(chain_spec).build().await?;
    let calldata = Bytes::from_static(&[0xde, 0xad, 0xbe, 0xef]);
    let request = BaseTransactionRequest::default()
        .from(Account::Alice.address())
        .to(address!("0000000000000000000000000000000000000004"))
        .input(TransactionInput::new(calldata.clone()));

    let result = harness.provider().call(request).block(BlockNumberOrTag::Latest.into()).await?;
    assert_eq!(result, calldata);

    Ok(())
}

#[tokio::test]
async fn azul_fill_transaction_without_data_uses_transfer_gas() -> Result<()> {
    let chain_spec = Arc::new(BaseChainSpec::from_genesis(build_test_genesis_v1()));
    let harness = TestHarnessBuilder::new().with_chain_spec(chain_spec).build().await?;
    let filled = harness.provider().fill_transaction(transfer_request_without_gas()).await?;
    assert!(!filled.raw.is_empty(), "filled raw transaction should not be empty");
    assert_eq!(filled.tx.gas_limit(), 21_000);

    Ok(())
}

#[tokio::test]
async fn azul_fill_transaction_with_data_accepts_implicit_gas_limit() -> Result<()> {
    let chain_spec = Arc::new(BaseChainSpec::from_genesis(build_test_genesis_v1()));
    let harness = TestHarnessBuilder::new().with_chain_spec(chain_spec).build().await?;
    let filled =
        harness.provider().fill_transaction(transfer_request_with_data_without_gas()).await?;
    assert!(!filled.raw.is_empty(), "filled raw transaction should not be empty");
    assert!(filled.tx.gas_limit() > 21_000, "tx with calldata should exceed plain transfer gas");

    Ok(())
}

#[tokio::test]
async fn azul_fill_transaction_long_calldata_accepts_implicit_gas_limit() -> Result<()> {
    let chain_spec = Arc::new(BaseChainSpec::from_genesis(build_test_genesis_v1()));
    let harness = TestHarnessBuilder::new().with_chain_spec(chain_spec).build().await?;
    let request = BaseTransactionRequest::default()
        .from(address!("1234567890abcdef1234567890abcdef12345678"))
        .to(address!("abcdef1234567890abcdef1234567890abcdef12"))
        .transaction_type(2u8)
        .input(TransactionInput::new(bytes!(
            "095ea7b3000000000000000000000000fedcba9876543210fedcba9876543210fedcba98000000000000000000000000000000000000000000000000000000000001e240"
        )));

    let filled = harness.provider().fill_transaction(request).await?;
    assert!(!filled.raw.is_empty(), "filled raw transaction should not be empty");
    assert!(filled.tx.gas_limit() > 21_000, "tx with calldata should exceed plain transfer gas");

    Ok(())
}
