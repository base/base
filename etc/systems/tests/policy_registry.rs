//! System tests for the policy registry precompile over Base node RPC.

mod common;

use alloy_primitives::{Address, LogData};
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::{SolCall, SolEvent};
use base_common_precompiles::{ActivationFeature, IPolicyRegistry, PolicyRegistryStorage};
use base_common_rpc_types::BaseTransactionReceipt;
use base_system_tests::{ANVIL_ACCOUNT_5, ANVIL_ACCOUNT_6, ANVIL_ACCOUNT_7, B20PrecompileClient};
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

/// Custom policy creation, membership, admin transfer, renounce, and error paths round-trip over RPC.
#[tokio::test]
async fn test_policy_registry_lifecycle_and_error_paths() -> Result<()> {
    let (_system, provider) = common::start_beryl_system().await?;
    let admin = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_5.private_key)
        .wrap_err("Failed to parse policy admin key")?;
    let next_admin = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_6.private_key)
        .wrap_err("Failed to parse next policy admin key")?;
    common::wait_for_balance(&provider, admin.address()).await?;
    common::wait_for_balance(&provider, next_admin.address()).await?;

    let client = B20PrecompileClient::new(&provider, &admin, common::L2_CHAIN_ID)
        .with_receipt_timeout(common::TX_RECEIPT_TIMEOUT);
    let next_admin_client = B20PrecompileClient::new(&provider, &next_admin, common::L2_CHAIN_ID)
        .with_receipt_timeout(common::TX_RECEIPT_TIMEOUT);
    client.activate_feature(ActivationFeature::PolicyRegistry.id()).await?;

    let allowlist_id = create_policy(
        &client,
        admin.address(),
        IPolicyRegistry::PolicyType::ALLOWLIST,
        "create allowlist policy",
    )
    .await?;
    assert!(policy_exists(&client, allowlist_id).await?, "new policy should exist");
    assert_eq!(policy_admin(&client, allowlist_id).await?, admin.address());
    assert_eq!(pending_policy_admin(&client, allowlist_id).await?, Address::ZERO);
    assert!(
        !is_authorized(&client, allowlist_id, ANVIL_ACCOUNT_7.address).await?,
        "fresh allowlist should reject non-members"
    );

    client
        .send_call(
            PolicyRegistryStorage::ADDRESS,
            IPolicyRegistry::updateAllowlistCall {
                policyId: allowlist_id,
                allowed: true,
                accounts: vec![ANVIL_ACCOUNT_7.address],
            },
            "update allowlist add account",
        )
        .await?;
    assert!(
        is_authorized(&client, allowlist_id, ANVIL_ACCOUNT_7.address).await?,
        "allowlisted account should be authorized"
    );

    let blocklist_id = create_policy_with_accounts(
        &client,
        admin.address(),
        IPolicyRegistry::PolicyType::BLOCKLIST,
        vec![ANVIL_ACCOUNT_7.address],
        "create blocklist policy with accounts",
    )
    .await?;
    assert!(
        !is_authorized(&client, blocklist_id, ANVIL_ACCOUNT_7.address).await?,
        "blocklisted account should not be authorized"
    );

    let allowlist_on_blocklist = client
        .try_send_call(
            PolicyRegistryStorage::ADDRESS,
            IPolicyRegistry::updateAllowlistCall {
                policyId: blocklist_id,
                allowed: true,
                accounts: vec![ANVIL_ACCOUNT_7.address],
            },
            "update allowlist on blocklist policy",
        )
        .await?;
    assert!(!allowlist_on_blocklist, "updateAllowlist on BLOCKLIST policy should revert");

    let blocklist_on_allowlist = client
        .try_send_call(
            PolicyRegistryStorage::ADDRESS,
            IPolicyRegistry::updateBlocklistCall {
                policyId: allowlist_id,
                blocked: true,
                accounts: vec![ANVIL_ACCOUNT_7.address],
            },
            "update blocklist on allowlist policy",
        )
        .await?;
    assert!(!blocklist_on_allowlist, "updateBlocklist on ALLOWLIST policy should revert");

    client
        .send_call(
            PolicyRegistryStorage::ADDRESS,
            IPolicyRegistry::stageUpdateAdminCall {
                policyId: allowlist_id,
                newAdmin: next_admin.address(),
            },
            "stage policy admin",
        )
        .await?;
    assert_eq!(pending_policy_admin(&client, allowlist_id).await?, next_admin.address());

    next_admin_client
        .send_call(
            PolicyRegistryStorage::ADDRESS,
            IPolicyRegistry::finalizeUpdateAdminCall { policyId: allowlist_id },
            "finalize policy admin",
        )
        .await?;
    assert_eq!(policy_admin(&client, allowlist_id).await?, next_admin.address());

    let old_admin_update = client
        .try_send_call(
            PolicyRegistryStorage::ADDRESS,
            IPolicyRegistry::updateAllowlistCall {
                policyId: allowlist_id,
                allowed: false,
                accounts: vec![ANVIL_ACCOUNT_7.address],
            },
            "old admin update allowlist",
        )
        .await?;
    assert!(!old_admin_update, "old admin update should revert after admin transfer");

    next_admin_client
        .send_call(
            PolicyRegistryStorage::ADDRESS,
            IPolicyRegistry::renounceAdminCall { policyId: allowlist_id },
            "renounce policy admin",
        )
        .await?;
    assert_eq!(policy_admin(&client, allowlist_id).await?, Address::ZERO);

    let renounced_update = next_admin_client
        .try_send_call(
            PolicyRegistryStorage::ADDRESS,
            IPolicyRegistry::updateAllowlistCall {
                policyId: allowlist_id,
                allowed: false,
                accounts: vec![ANVIL_ACCOUNT_7.address],
            },
            "renounced policy update allowlist",
        )
        .await?;
    assert!(!renounced_update, "renounced policy should be frozen");

    let zero_admin_create = client
        .try_send_call(
            PolicyRegistryStorage::ADDRESS,
            IPolicyRegistry::createPolicyCall {
                admin: Address::ZERO,
                policyType: IPolicyRegistry::PolicyType::ALLOWLIST,
            },
            "create policy with zero admin",
        )
        .await?;
    assert!(!zero_admin_create, "createPolicy with zero admin should revert");

    let no_pending = client
        .try_send_call(
            PolicyRegistryStorage::ADDRESS,
            IPolicyRegistry::finalizeUpdateAdminCall { policyId: allowlist_id },
            "finalize without pending admin",
        )
        .await?;
    assert!(!no_pending, "finalizeUpdateAdmin without pending admin should revert");

    Ok(())
}

/// View calls remain available while write calls are gated when the policy registry is deactivated.
#[tokio::test]
async fn test_policy_registry_deactivated_views_and_write_gate() -> Result<()> {
    let (_system, provider) = common::start_beryl_system().await?;
    let admin = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_5.private_key)
        .wrap_err("Failed to parse policy admin key")?;
    common::wait_for_balance(&provider, admin.address()).await?;

    let client = B20PrecompileClient::new(&provider, &admin, common::L2_CHAIN_ID)
        .with_receipt_timeout(common::TX_RECEIPT_TIMEOUT);
    client.activate_feature(ActivationFeature::PolicyRegistry.id()).await?;

    let blocklist_id = create_policy(
        &client,
        admin.address(),
        IPolicyRegistry::PolicyType::BLOCKLIST,
        "create blocklist policy",
    )
    .await?;
    client
        .send_call(
            PolicyRegistryStorage::ADDRESS,
            IPolicyRegistry::updateBlocklistCall {
                policyId: blocklist_id,
                blocked: true,
                accounts: vec![ANVIL_ACCOUNT_6.address],
            },
            "update blocklist add account",
        )
        .await?;

    client.deactivate_feature(ActivationFeature::PolicyRegistry.id()).await?;
    assert!(policy_exists(&client, blocklist_id).await?, "view policyExists should still succeed");
    assert_eq!(policy_admin(&client, blocklist_id).await?, admin.address());
    assert!(
        !is_authorized(&client, blocklist_id, ANVIL_ACCOUNT_6.address).await?,
        "view isAuthorized should preserve blocked account state while deactivated"
    );

    let create_while_deactivated = client
        .try_send_call(
            PolicyRegistryStorage::ADDRESS,
            IPolicyRegistry::createPolicyCall {
                admin: admin.address(),
                policyType: IPolicyRegistry::PolicyType::ALLOWLIST,
            },
            "create policy while deactivated",
        )
        .await?;
    assert!(!create_while_deactivated, "policy registry writes should revert while deactivated");

    Ok(())
}

async fn create_policy(
    client: &B20PrecompileClient<'_>,
    admin: Address,
    policy_type: IPolicyRegistry::PolicyType,
    label: &'static str,
) -> Result<u64> {
    let call = IPolicyRegistry::createPolicyCall { admin, policyType: policy_type };
    let output = client.call(PolicyRegistryStorage::ADDRESS, call.clone()).await?;
    let policy_id = IPolicyRegistry::createPolicyCall::abi_decode_returns(output.as_ref())
        .wrap_err("Failed to decode createPolicy")?;
    client.send_call(PolicyRegistryStorage::ADDRESS, call, label).await?;
    Ok(policy_id)
}

async fn create_policy_with_accounts(
    client: &B20PrecompileClient<'_>,
    admin: Address,
    policy_type: IPolicyRegistry::PolicyType,
    accounts: Vec<Address>,
    label: &'static str,
) -> Result<u64> {
    let call =
        IPolicyRegistry::createPolicyWithAccountsCall { admin, policyType: policy_type, accounts };
    let output = client.call(PolicyRegistryStorage::ADDRESS, call.clone()).await?;
    let policy_id =
        IPolicyRegistry::createPolicyWithAccountsCall::abi_decode_returns(output.as_ref())
            .wrap_err("Failed to decode createPolicyWithAccounts")?;
    client.send_call(PolicyRegistryStorage::ADDRESS, call, label).await?;
    Ok(policy_id)
}

async fn policy_exists(client: &B20PrecompileClient<'_>, policy_id: u64) -> Result<bool> {
    let output = client
        .call(
            PolicyRegistryStorage::ADDRESS,
            IPolicyRegistry::policyExistsCall { policyId: policy_id },
        )
        .await?;
    IPolicyRegistry::policyExistsCall::abi_decode_returns(output.as_ref())
        .wrap_err("Failed to decode policyExists")
}

async fn policy_admin(client: &B20PrecompileClient<'_>, policy_id: u64) -> Result<Address> {
    let output = client
        .call(
            PolicyRegistryStorage::ADDRESS,
            IPolicyRegistry::policyAdminCall { policyId: policy_id },
        )
        .await?;
    IPolicyRegistry::policyAdminCall::abi_decode_returns(output.as_ref())
        .wrap_err("Failed to decode policyAdmin")
}

async fn pending_policy_admin(client: &B20PrecompileClient<'_>, policy_id: u64) -> Result<Address> {
    let output = client
        .call(
            PolicyRegistryStorage::ADDRESS,
            IPolicyRegistry::pendingPolicyAdminCall { policyId: policy_id },
        )
        .await?;
    IPolicyRegistry::pendingPolicyAdminCall::abi_decode_returns(output.as_ref())
        .wrap_err("Failed to decode pendingPolicyAdmin")
}

async fn is_authorized(
    client: &B20PrecompileClient<'_>,
    policy_id: u64,
    account: Address,
) -> Result<bool> {
    let output = client
        .call(
            PolicyRegistryStorage::ADDRESS,
            IPolicyRegistry::isAuthorizedCall { policyId: policy_id, account },
        )
        .await?;
    IPolicyRegistry::isAuthorizedCall::abi_decode_returns(output.as_ref())
        .wrap_err("Failed to decode isAuthorized")
}

fn assert_receipt_log(receipt: &BaseTransactionReceipt, address: Address, expected: LogData) {
    assert!(
        receipt.inner.logs().iter().any(|log| log.address() == address && log.data() == &expected),
        "receipt must contain expected log at {address}; expected={expected:?}, logs={:?}",
        receipt.inner.logs()
    );
}
