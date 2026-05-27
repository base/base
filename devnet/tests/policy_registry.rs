//! End-to-end tests for the policy registry precompile over Base node RPC.

mod common;

use alloy_primitives::Address;
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::SolCall;
use base_common_precompiles::{ActivationFeature, IPolicyRegistry, PolicyRegistryStorage};
use devnet::{
    B20PrecompileClient,
    config::{ANVIL_ACCOUNT_5, ANVIL_ACCOUNT_6},
};
use eyre::{Result, WrapErr};

/// `policyExists(ALWAYS_ALLOW_ID)` returns `true` once the policy registry is active.
#[tokio::test]
async fn test_policy_registry_policy_exists() -> Result<()> {
    let (_devnet, provider) = common::start_beryl_devnet().await?;
    let caller = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_5.private_key)
        .wrap_err("Failed to parse devnet private key")?;
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

/// Full admin lifecycle over RPC: create policy, stage and finalize admin handoff, verify new
/// admin can mutate and old admin cannot, renounce admin, verify the policy is frozen, and
/// verify read-only views still work after renounce.
#[tokio::test]
async fn test_policy_registry_admin_handoff_and_frozen_policy() -> Result<()> {
    let (_devnet, provider) = common::start_beryl_devnet().await?;
    let admin = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_5.private_key)
        .wrap_err("Failed to parse devnet private key")?;
    let new_admin = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_6.private_key)
        .wrap_err("Failed to parse new admin private key")?;
    common::wait_for_balance(&provider, admin.address()).await?;
    common::wait_for_balance(&provider, new_admin.address()).await?;

    let admin_client = B20PrecompileClient::new(&provider, &admin, common::L2_CHAIN_ID)
        .with_receipt_timeout(common::TX_RECEIPT_TIMEOUT);
    let new_admin_client = B20PrecompileClient::new(&provider, &new_admin, common::L2_CHAIN_ID)
        .with_receipt_timeout(common::TX_RECEIPT_TIMEOUT);

    // Activate the PolicyRegistry feature.
    admin_client.activate_feature(ActivationFeature::PolicyRegistry.id()).await?;

    // Simulate createPolicy to get the ID the registry will assign, then dispatch the real
    // transaction. Using call() avoids relying on internal counter state or enum discriminant
    // ordering.
    let create_call = IPolicyRegistry::createPolicyCall {
        admin: admin.address(),
        policyType: IPolicyRegistry::PolicyType::ALLOWLIST,
    };
    let output =
        admin_client.call(PolicyRegistryStorage::ADDRESS, create_call.clone()).await?;
    let policy_id = IPolicyRegistry::createPolicyCall::abi_decode_returns(output.as_ref())
        .wrap_err("Failed to decode createPolicy return")?;
    admin_client
        .send_call(PolicyRegistryStorage::ADDRESS, create_call, "createPolicy")
        .await?;

    // Stage the admin transfer to new_admin.
    admin_client
        .send_call(
            PolicyRegistryStorage::ADDRESS,
            IPolicyRegistry::stageUpdateAdminCall {
                policyId: policy_id,
                newAdmin: new_admin.address(),
            },
            "stageUpdateAdmin",
        )
        .await?;

    // Finalize the admin transfer from new_admin.
    new_admin_client
        .send_call(
            PolicyRegistryStorage::ADDRESS,
            IPolicyRegistry::finalizeUpdateAdminCall { policyId: policy_id },
            "finalizeUpdateAdmin",
        )
        .await?;

    // Verify policyAdmin now returns new_admin.
    let output = admin_client
        .call(
            PolicyRegistryStorage::ADDRESS,
            IPolicyRegistry::policyAdminCall { policyId: policy_id },
        )
        .await?;
    let current_admin = IPolicyRegistry::policyAdminCall::abi_decode_returns(output.as_ref())
        .wrap_err("Failed to decode policyAdmin")?;
    assert_eq!(current_admin, new_admin.address(), "policyAdmin should be new_admin after handoff");

    // Verify new admin can mutate: add an address to the allowlist.
    let allowlisted = Address::repeat_byte(0xaa);
    new_admin_client
        .send_call(
            PolicyRegistryStorage::ADDRESS,
            IPolicyRegistry::updateAllowlistCall {
                policyId: policy_id,
                allowed: true,
                accounts: vec![allowlisted],
            },
            "updateAllowlist (new admin)",
        )
        .await?;

    // Verify old admin can no longer mutate after the handoff.
    let succeeded = admin_client
        .try_send_call(
            PolicyRegistryStorage::ADDRESS,
            IPolicyRegistry::updateAllowlistCall {
                policyId: policy_id,
                allowed: true,
                accounts: vec![Address::repeat_byte(0xbb)],
            },
            "updateAllowlist from old admin (should revert)",
        )
        .await?;
    assert!(!succeeded, "updateAllowlist from old admin should revert after handoff");

    // Renounce admin: new_admin gives up control permanently.
    new_admin_client
        .send_call(
            PolicyRegistryStorage::ADDRESS,
            IPolicyRegistry::renounceAdminCall { policyId: policy_id },
            "renounceAdmin",
        )
        .await?;

    // Policy is now frozen: updateAllowlist from anyone reverts.
    let succeeded = new_admin_client
        .try_send_call(
            PolicyRegistryStorage::ADDRESS,
            IPolicyRegistry::updateAllowlistCall {
                policyId: policy_id,
                allowed: false,
                accounts: vec![allowlisted],
            },
            "updateAllowlist after renounce (should revert)",
        )
        .await?;
    assert!(!succeeded, "updateAllowlist should revert after renounceAdmin");

    // Read-only views must still work correctly after renounce.
    let output = admin_client
        .call(
            PolicyRegistryStorage::ADDRESS,
            IPolicyRegistry::isAuthorizedCall { policyId: policy_id, account: allowlisted },
        )
        .await?;
    let is_auth = IPolicyRegistry::isAuthorizedCall::abi_decode_returns(output.as_ref())
        .wrap_err("Failed to decode isAuthorized")?;
    assert!(
        is_auth,
        "isAuthorized should still return true for allowlisted address after renounce",
    );

    let output = admin_client
        .call(
            PolicyRegistryStorage::ADDRESS,
            IPolicyRegistry::policyExistsCall { policyId: policy_id },
        )
        .await?;
    let exists = IPolicyRegistry::policyExistsCall::abi_decode_returns(output.as_ref())
        .wrap_err("Failed to decode policyExists")?;
    assert!(exists, "policyExists should still return true after renounceAdmin");

    Ok(())
}
