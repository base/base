//! System tests for the policy registry precompile over Base node RPC.

mod common;

use alloy_primitives::{Address, LogData};
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::{SolCall, SolEvent};
use base_common_precompiles::{ActivationFeature, IPolicyRegistry, PolicyRegistryStorage};
use base_common_rpc_types::BaseTransactionReceipt;
use base_system_tests::{ANVIL_ACCOUNT_5, B20PrecompileClient};
use eyre::{Result, WrapErr};

/// `createPolicy` emits the expected policy-registry events over RPC receipts.
#[tokio::test]
async fn test_policy_registry_create_policy_emits_events() -> Result<()> {
    let (_system, provider) = common::start_beryl_system().await?;
    let caller = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_5.private_key)
        .wrap_err("Failed to parse system test private key")?;
    common::wait_for_balance(&provider, caller.address()).await?;

    let client = B20PrecompileClient::new(&provider, &caller, common::L2_CHAIN_ID)
        .with_receipt_timeout(common::TX_RECEIPT_TIMEOUT);
    client.activate_feature(ActivationFeature::PolicyRegistry.id()).await?;

    let call = IPolicyRegistry::createPolicyCall {
        admin: caller.address(),
        policyType: IPolicyRegistry::PolicyType::ALLOWLIST,
    };
    let output = client.call(PolicyRegistryStorage::ADDRESS, call.clone()).await?;
    let policy_id = IPolicyRegistry::createPolicyCall::abi_decode_returns(output.as_ref())
        .wrap_err("Failed to decode createPolicy")?;
    let receipt = client
        .send_call_receipt(PolicyRegistryStorage::ADDRESS, call, "createPolicy ALLOWLIST")
        .await?;

    assert_receipt_log(
        &receipt,
        PolicyRegistryStorage::ADDRESS,
        IPolicyRegistry::PolicyCreated {
            policyId: policy_id,
            creator: caller.address(),
            policyType: IPolicyRegistry::PolicyType::ALLOWLIST,
        }
        .encode_log_data(),
    );
    assert_receipt_log(
        &receipt,
        PolicyRegistryStorage::ADDRESS,
        IPolicyRegistry::PolicyAdminUpdated {
            policyId: policy_id,
            previousAdmin: Address::ZERO,
            newAdmin: caller.address(),
        }
        .encode_log_data(),
    );

    Ok(())
}

/// `policyExists(ALWAYS_ALLOW_ID)` returns `true` once the policy registry is active.
#[tokio::test]
async fn test_policy_registry_policy_exists() -> Result<()> {
    let (_system, provider) = common::start_beryl_system().await?;
    let caller = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_5.private_key)
        .wrap_err("Failed to parse system test private key")?;
    common::wait_for_balance(&provider, caller.address()).await?;

    let client = B20PrecompileClient::new(&provider, &caller, common::L2_CHAIN_ID)
        .with_receipt_timeout(common::TX_RECEIPT_TIMEOUT);
    client.activate_feature(ActivationFeature::PolicyRegistry.id()).await?;

    let output = client
        .call(
            PolicyRegistryStorage::ADDRESS,
            IPolicyRegistry::policyExistsCall { policyId: PolicyRegistryStorage::ALWAYS_ALLOW_ID },
        )
        .await?;
    let result = IPolicyRegistry::policyExistsCall::abi_decode_returns(output.as_ref())
        .wrap_err("Failed to decode policyExists")?;

    assert!(result, "policyExists(0) should return true after Beryl activation");

    Ok(())
}

fn assert_receipt_log(receipt: &BaseTransactionReceipt, address: Address, expected: LogData) {
    assert!(
        receipt.inner.logs().iter().any(|log| log.address() == address && log.data() == &expected),
        "receipt must contain expected log at {address}; expected={expected:?}, logs={:?}",
        receipt.inner.logs()
    );
}
