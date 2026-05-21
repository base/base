//! End-to-end tests for the policy registry precompile over Base node RPC.

mod common;

use alloy_primitives::Address;
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::SolCall;
use base_common_precompiles::{IPolicyRegistry, PolicyRegistryStorage};
use devnet::{
    B20PrecompileClient,
    config::{ANVIL_ACCOUNT_5, ANVIL_ACCOUNT_6, ANVIL_ACCOUNT_7, ANVIL_ACCOUNT_8},
};
use eyre::{Result, WrapErr};

// --- read helpers ---

async fn read_next_policy_id(
    client: &B20PrecompileClient<'_>,
    policy_type: IPolicyRegistry::PolicyType,
) -> Result<u64> {
    let out = client
        .call(
            PolicyRegistryStorage::ADDRESS,
            IPolicyRegistry::nextPolicyIdCall { policyType: policy_type },
        )
        .await?;
    IPolicyRegistry::nextPolicyIdCall::abi_decode_returns(out.as_ref())
        .wrap_err("Failed to decode nextPolicyId")
}

async fn read_is_authorized(
    client: &B20PrecompileClient<'_>,
    policy_id: u64,
    account: Address,
) -> Result<bool> {
    let out = client
        .call(
            PolicyRegistryStorage::ADDRESS,
            IPolicyRegistry::isAuthorizedCall { policyId: policy_id, account },
        )
        .await?;
    IPolicyRegistry::isAuthorizedCall::abi_decode_returns(out.as_ref())
        .wrap_err("Failed to decode isAuthorized")
}

async fn read_policy_admin(
    client: &B20PrecompileClient<'_>,
    policy_id: u64,
) -> Result<Address> {
    let out = client
        .call(
            PolicyRegistryStorage::ADDRESS,
            IPolicyRegistry::policyAdminCall { policyId: policy_id },
        )
        .await?;
    IPolicyRegistry::policyAdminCall::abi_decode_returns(out.as_ref())
        .wrap_err("Failed to decode policyAdmin")
}

async fn read_pending_policy_admin(
    client: &B20PrecompileClient<'_>,
    policy_id: u64,
) -> Result<Address> {
    let out = client
        .call(
            PolicyRegistryStorage::ADDRESS,
            IPolicyRegistry::pendingPolicyAdminCall { policyId: policy_id },
        )
        .await?;
    IPolicyRegistry::pendingPolicyAdminCall::abi_decode_returns(out.as_ref())
        .wrap_err("Failed to decode pendingPolicyAdmin")
}

async fn read_policy_type(
    client: &B20PrecompileClient<'_>,
    policy_id: u64,
) -> Result<IPolicyRegistry::PolicyType> {
    let out = client
        .call(
            PolicyRegistryStorage::ADDRESS,
            IPolicyRegistry::policyTypeCall { policyId: policy_id },
        )
        .await?;
    IPolicyRegistry::policyTypeCall::abi_decode_returns(out.as_ref())
        .wrap_err("Failed to decode policyType")
}

// --- existing test ---

/// `policyExists(0)` returns `true` once the Beryl fork is active.
#[tokio::test]
async fn test_policy_registry_policy_exists() -> Result<()> {
    let (_devnet, provider) = common::start_beryl_devnet().await?;
    let caller = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_5.private_key)
        .wrap_err("Failed to parse devnet private key")?;
    common::wait_for_balance(&provider, caller.address()).await?;

    let client = B20PrecompileClient::new(&provider, &caller, common::L2_CHAIN_ID)
        .with_receipt_timeout(common::TX_RECEIPT_TIMEOUT);

    let output = client
        .call(PolicyRegistryStorage::ADDRESS, IPolicyRegistry::policyExistsCall { policyId: 0 })
        .await?;
    let result = IPolicyRegistry::policyExistsCall::abi_decode_returns(output.as_ref())
        .wrap_err("Failed to decode policyExists")?;

    assert!(result, "policyExists(0) should return true after Beryl activation");

    Ok(())
}

// --- new lifecycle tests ---

/// Creates an ALLOWLIST policy, adds a member via `updateAllowlist`, and checks `isAuthorized`.
///
/// Verifies that the member is authorized and a non-member is not.
#[tokio::test]
async fn test_create_allowlist_policy_and_authorize() -> Result<()> {
    let (_devnet, provider) = common::start_beryl_devnet().await?;
    let admin =
        PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_5.private_key).wrap_err("admin key")?;
    let member = ANVIL_ACCOUNT_7.address;
    let non_member = ANVIL_ACCOUNT_8.address;
    common::wait_for_balance(&provider, admin.address()).await?;

    let client = B20PrecompileClient::new(&provider, &admin, common::L2_CHAIN_ID)
        .with_receipt_timeout(common::TX_RECEIPT_TIMEOUT);

    let policy_id =
        read_next_policy_id(&client, IPolicyRegistry::PolicyType::ALLOWLIST).await?;

    client
        .send_call(
            PolicyRegistryStorage::ADDRESS,
            IPolicyRegistry::createPolicyCall {
                admin: admin.address(),
                policyType: IPolicyRegistry::PolicyType::ALLOWLIST,
            },
            "createPolicy(ALLOWLIST)",
        )
        .await?;

    client
        .send_call(
            PolicyRegistryStorage::ADDRESS,
            IPolicyRegistry::updateAllowlistCall {
                policyId: policy_id,
                allowed: true,
                accounts: vec![member],
            },
            "updateAllowlist(add member)",
        )
        .await?;

    assert!(
        read_is_authorized(&client, policy_id, member).await?,
        "allowlist member should be authorized",
    );
    assert!(
        !read_is_authorized(&client, policy_id, non_member).await?,
        "non-member should not be authorized on allowlist policy",
    );

    Ok(())
}

/// Creates a BLOCKLIST policy, adds a member via `updateBlocklist`, and checks `isAuthorized`.
///
/// Verifies that the blocked account is not authorized while an unlisted account is.
#[tokio::test]
async fn test_create_blocklist_policy_and_block() -> Result<()> {
    let (_devnet, provider) = common::start_beryl_devnet().await?;
    let admin =
        PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_5.private_key).wrap_err("admin key")?;
    let blocked = ANVIL_ACCOUNT_7.address;
    let non_blocked = ANVIL_ACCOUNT_8.address;
    common::wait_for_balance(&provider, admin.address()).await?;

    let client = B20PrecompileClient::new(&provider, &admin, common::L2_CHAIN_ID)
        .with_receipt_timeout(common::TX_RECEIPT_TIMEOUT);

    let policy_id =
        read_next_policy_id(&client, IPolicyRegistry::PolicyType::BLOCKLIST).await?;

    client
        .send_call(
            PolicyRegistryStorage::ADDRESS,
            IPolicyRegistry::createPolicyCall {
                admin: admin.address(),
                policyType: IPolicyRegistry::PolicyType::BLOCKLIST,
            },
            "createPolicy(BLOCKLIST)",
        )
        .await?;

    client
        .send_call(
            PolicyRegistryStorage::ADDRESS,
            IPolicyRegistry::updateBlocklistCall {
                policyId: policy_id,
                blocked: true,
                accounts: vec![blocked],
            },
            "updateBlocklist(add blocked)",
        )
        .await?;

    assert!(
        !read_is_authorized(&client, policy_id, blocked).await?,
        "blocklisted account should not be authorized",
    );
    assert!(
        read_is_authorized(&client, policy_id, non_blocked).await?,
        "non-blocked account should be authorized on blocklist policy",
    );

    Ok(())
}

/// Exercises the two-step admin transfer: `stageUpdateAdmin` then `finalizeUpdateAdmin`.
///
/// Verifies that `policyAdmin` updates to the new admin and the pending slot is cleared.
#[tokio::test]
async fn test_two_step_admin_transfer() -> Result<()> {
    let (_devnet, provider) = common::start_beryl_devnet().await?;
    let admin =
        PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_5.private_key).wrap_err("admin key")?;
    let new_admin =
        PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_6.private_key).wrap_err("new_admin key")?;
    common::wait_for_balance(&provider, admin.address()).await?;
    common::wait_for_balance(&provider, new_admin.address()).await?;

    let client_admin = B20PrecompileClient::new(&provider, &admin, common::L2_CHAIN_ID)
        .with_receipt_timeout(common::TX_RECEIPT_TIMEOUT);
    let client_new_admin = B20PrecompileClient::new(&provider, &new_admin, common::L2_CHAIN_ID)
        .with_receipt_timeout(common::TX_RECEIPT_TIMEOUT);

    let policy_id =
        read_next_policy_id(&client_admin, IPolicyRegistry::PolicyType::ALLOWLIST).await?;

    client_admin
        .send_call(
            PolicyRegistryStorage::ADDRESS,
            IPolicyRegistry::createPolicyCall {
                admin: admin.address(),
                policyType: IPolicyRegistry::PolicyType::ALLOWLIST,
            },
            "createPolicy(ALLOWLIST)",
        )
        .await?;

    assert_eq!(
        read_policy_admin(&client_admin, policy_id).await?,
        admin.address(),
        "policyAdmin should be the creator after createPolicy",
    );

    client_admin
        .send_call(
            PolicyRegistryStorage::ADDRESS,
            IPolicyRegistry::stageUpdateAdminCall {
                policyId: policy_id,
                newAdmin: new_admin.address(),
            },
            "stageUpdateAdmin",
        )
        .await?;

    assert_eq!(
        read_pending_policy_admin(&client_admin, policy_id).await?,
        new_admin.address(),
        "pendingPolicyAdmin should be set after stageUpdateAdmin",
    );

    client_new_admin
        .send_call(
            PolicyRegistryStorage::ADDRESS,
            IPolicyRegistry::finalizeUpdateAdminCall { policyId: policy_id },
            "finalizeUpdateAdmin",
        )
        .await?;

    assert_eq!(
        read_policy_admin(&client_admin, policy_id).await?,
        new_admin.address(),
        "policyAdmin should be new_admin after finalizeUpdateAdmin",
    );
    assert_eq!(
        read_pending_policy_admin(&client_admin, policy_id).await?,
        Address::ZERO,
        "pendingPolicyAdmin should be cleared after finalizeUpdateAdmin",
    );

    Ok(())
}

/// Creates a policy and calls `renounceAdmin`, verifying `policyAdmin` becomes the zero address.
#[tokio::test]
async fn test_renounce_admin() -> Result<()> {
    let (_devnet, provider) = common::start_beryl_devnet().await?;
    let admin =
        PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_5.private_key).wrap_err("admin key")?;
    common::wait_for_balance(&provider, admin.address()).await?;

    let client = B20PrecompileClient::new(&provider, &admin, common::L2_CHAIN_ID)
        .with_receipt_timeout(common::TX_RECEIPT_TIMEOUT);

    let policy_id =
        read_next_policy_id(&client, IPolicyRegistry::PolicyType::ALLOWLIST).await?;

    client
        .send_call(
            PolicyRegistryStorage::ADDRESS,
            IPolicyRegistry::createPolicyCall {
                admin: admin.address(),
                policyType: IPolicyRegistry::PolicyType::ALLOWLIST,
            },
            "createPolicy(ALLOWLIST)",
        )
        .await?;

    client
        .send_call(
            PolicyRegistryStorage::ADDRESS,
            IPolicyRegistry::renounceAdminCall { policyId: policy_id },
            "renounceAdmin",
        )
        .await?;

    assert_eq!(
        read_policy_admin(&client, policy_id).await?,
        Address::ZERO,
        "policyAdmin should be zero address after renounceAdmin",
    );

    Ok(())
}

/// Verifies that `nextPolicyId`, `policyType`, `policyAdmin`, and `pendingPolicyAdmin` all return
/// correct values after policy creation.
#[tokio::test]
async fn test_policy_views() -> Result<()> {
    let (_devnet, provider) = common::start_beryl_devnet().await?;
    let admin =
        PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_5.private_key).wrap_err("admin key")?;
    common::wait_for_balance(&provider, admin.address()).await?;

    let client = B20PrecompileClient::new(&provider, &admin, common::L2_CHAIN_ID)
        .with_receipt_timeout(common::TX_RECEIPT_TIMEOUT);

    // Snapshot the predicted policy ID before any creation.
    let predicted_id =
        read_next_policy_id(&client, IPolicyRegistry::PolicyType::ALLOWLIST).await?;

    client
        .send_call(
            PolicyRegistryStorage::ADDRESS,
            IPolicyRegistry::createPolicyCall {
                admin: admin.address(),
                policyType: IPolicyRegistry::PolicyType::ALLOWLIST,
            },
            "createPolicy(ALLOWLIST)",
        )
        .await?;

    assert_eq!(
        read_policy_type(&client, predicted_id).await?,
        IPolicyRegistry::PolicyType::ALLOWLIST,
        "policyType should be ALLOWLIST",
    );

    assert_eq!(
        read_policy_admin(&client, predicted_id).await?,
        admin.address(),
        "policyAdmin should match the admin set at creation",
    );

    assert_eq!(
        read_pending_policy_admin(&client, predicted_id).await?,
        Address::ZERO,
        "pendingPolicyAdmin should be zero before any staging",
    );

    // The counter must have advanced so the next ID differs from the one we just created.
    let next_id_after =
        read_next_policy_id(&client, IPolicyRegistry::PolicyType::ALLOWLIST).await?;
    assert_ne!(
        next_id_after, predicted_id,
        "nextPolicyId should advance after createPolicy",
    );

    Ok(())
}
