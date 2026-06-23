//! Behavioural regression tests for the standalone EIP-8130
//! `eth_getTransactionCount` override.
//!
//! These tests pin the two special-case dispatch branches in
//! `ChannelNonceReader::read` — protocol-nonce delegation for `nonce_key == 0`
//! and `INVALID_PARAMS` for the `NONCE_KEY_MAX` sentinel — by exercising the
//! full RPC path against a Cobalt-activated test harness.

use std::sync::Arc;

use alloy_primitives::{Address, U256};
use alloy_rpc_client::RpcClient;
use base_common_consensus::Eip8130Constants;
use base_execution_chainspec::BaseChainSpec;
use base_execution_eip8130_rpc_node::{Eip8130RpcExtension, Eip8130RpcMode};
use base_node_runner::test_utils::TestHarness;
use base_test_utils::{Account, build_test_genesis_cobalt};

async fn setup() -> eyre::Result<(TestHarness, RpcClient)> {
    let chain_spec = Arc::new(BaseChainSpec::from_genesis(build_test_genesis_cobalt()));
    let harness = TestHarness::builder()
        .with_chain_spec(chain_spec)
        .with_ext::<Eip8130RpcExtension>(Eip8130RpcMode::Register)
        .build()
        .await?;
    let client = harness.rpc_client()?;
    Ok((harness, client))
}

/// `nonce_key == 0` must delegate to the standard protocol-nonce path
/// (`EthState::transaction_count`) rather than reading the precompile.
#[tokio::test]
async fn nonce_key_zero_returns_protocol_nonce() -> eyre::Result<()> {
    let (_harness, client) = setup().await?;
    let alice: Address = Account::Alice.address();

    let legacy: U256 = client.request("eth_getTransactionCount", (alice, "latest")).await?;
    let with_key: U256 =
        client.request("eth_getTransactionCount", (alice, "latest", U256::ZERO)).await?;

    assert_eq!(with_key, legacy, "nonce_key=0 must return the same value as the legacy 2-arg call");
    Ok(())
}

/// `nonce_key == NONCE_KEY_MAX` must return `INVALID_PARAMS` because the
/// expiring-nonce sentinel has no per-channel counter; replay protection
/// there relies on `expiry`, not a sequence number.
#[tokio::test]
async fn nonce_key_max_returns_invalid_params() -> eyre::Result<()> {
    let (_harness, client) = setup().await?;
    let alice: Address = Account::Alice.address();

    let result: Result<U256, _> = client
        .request("eth_getTransactionCount", (alice, "latest", Eip8130Constants::NONCE_KEY_MAX))
        .await;

    let err = result.expect_err("NONCE_KEY_MAX must error");
    let err_str = err.to_string();
    assert!(err_str.contains("-32602"), "expected INVALID_PARAMS (-32602), got: {err_str}");
    Ok(())
}
