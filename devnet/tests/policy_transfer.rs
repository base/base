//! End-to-end tests for policy-gated B20 token transfers over Base node RPC.
//!
//! Each test:
//!   - Creates a policy in the policy registry via RPC.
//!   - Creates a B20 token and wires the policy to its `TRANSFER_SENDER_POLICY`
//!     slot via `updatePolicy`.
//!   - Exercises the full transfer-gate cycle: blocked → allowed (or vice versa).

mod common;

use alloy_primitives::{Address, B256, U256};
use alloy_provider::RootProvider;
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::SolCall;
use base_alloy_network::Base;
use base_common_precompiles::{
    ActivationFeature, B20PolicyType, B20Variant, IB20, IPolicyRegistry, PolicyRegistryStorage,
};
use devnet::{
    B20PrecompileClient,
    config::{
        ANVIL_ACCOUNT_0, ANVIL_ACCOUNT_1, ANVIL_ACCOUNT_2, ANVIL_ACCOUNT_3, ANVIL_ACCOUNT_4,
        ANVIL_ACCOUNT_5, ANVIL_ACCOUNT_6, ANVIL_ACCOUNT_7, ANVIL_ACCOUNT_8,
    },
};
use eyre::{Result, WrapErr};

const INITIAL_SUPPLY: u64 = 1_000_000;
const TRANSFER_AMOUNT: u64 = 100_000;

// Salts must not overlap with those used in b20_precompile.rs (0x10–0x18, 0x42).
const SALT_ALLOWLIST: B256 = B256::repeat_byte(0x50);
const SALT_BLOCKLIST: B256 = B256::repeat_byte(0x51);
const SALT_ALWAYS_BLOCK: B256 = B256::repeat_byte(0x52);
const SALT_PERSIST_BLOCKS: B256 = B256::repeat_byte(0x53);
const SALT_DEFAULT_ALLOW: B256 = B256::repeat_byte(0x54);

/// Activates `B20_FACTORY`, `B20_TOKEN`, and `POLICY_REGISTRY` features, then
/// returns a [`B20PrecompileClient`] ready for precompile calls.
async fn activated_client<'a>(
    provider: &'a RootProvider<Base>,
    admin: &'a PrivateKeySigner,
) -> Result<B20PrecompileClient<'a>> {
    let client = B20PrecompileClient::new(provider, admin, common::L2_CHAIN_ID)
        .with_receipt_timeout(common::TX_RECEIPT_TIMEOUT);
    client.activate_feature(ActivationFeature::B20Factory.id()).await?;
    client.activate_feature(ActivationFeature::B20Token.id()).await?;
    client.activate_feature(ActivationFeature::PolicyRegistry.id()).await?;
    Ok(client)
}

/// Creates a policy and returns its assigned ID.
///
/// Simulates the call first (`eth_call`) to obtain the ID the registry will
/// assign, then dispatches the real transaction.  Because the devnet is
/// single-sender the counter cannot advance between the simulation and the
/// actual transaction.
async fn create_policy(
    client: &B20PrecompileClient<'_>,
    admin: Address,
    policy_type: IPolicyRegistry::PolicyType,
    label: &'static str,
) -> Result<u64> {
    let call = IPolicyRegistry::createPolicyCall { admin, policyType: policy_type };
    let output = client.call(PolicyRegistryStorage::ADDRESS, call.clone()).await?;
    let policy_id = IPolicyRegistry::createPolicyCall::abi_decode_returns(output.as_ref())
        .wrap_err("Failed to decode createPolicy return")?;
    client.send_call(PolicyRegistryStorage::ADDRESS, call, label).await?;
    Ok(policy_id)
}

/// Queries `isAuthorized(policy_id, account)` from the policy registry.
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

/// Creates a B20 token with an initial supply minted to `admin`.
async fn create_token(
    client: &B20PrecompileClient<'_>,
    admin: Address,
    salt: B256,
    name: &str,
    symbol: &str,
) -> Result<Address> {
    let params =
        B20PrecompileClient::token_params(name, symbol, admin, U256::from(INITIAL_SUPPLY), admin);
    let token = client.create_token(B20Variant::B20, params, salt).await?;
    client
        .wait_for_token_code(token, common::TX_RECEIPT_TIMEOUT, common::BLOCK_POLL_INTERVAL)
        .await?;
    Ok(token)
}

/// Wires a policy ID to the token's `TRANSFER_SENDER_POLICY` slot.
async fn set_transfer_sender_policy(
    client: &B20PrecompileClient<'_>,
    token: Address,
    policy_id: u64,
) -> Result<()> {
    client
        .send_call(
            token,
            IB20::updatePolicyCall {
                policyScope: B20PolicyType::TransferSender.id(),
                newPolicyId: policy_id,
            },
            "updatePolicy TRANSFER_SENDER_POLICY",
        )
        .await
}

/// `test_allowlist_gates_transfer`
///
/// Full cycle:
///   1. Create an ALLOWLIST policy.
///   2. Wire it to the token's `TRANSFER_SENDER_POLICY` slot.
///   3. Assert a non-member transfer reverts.
///   4. Add the non-member to the allowlist.
///   5. Assert the transfer now succeeds.
#[tokio::test]
async fn test_allowlist_gates_transfer() -> Result<()> {
    let (_devnet, provider) = common::start_beryl_devnet().await?;

    let admin = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_5.private_key).wrap_err("admin key")?;
    let non_member =
        PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_7.private_key).wrap_err("non-member key")?;
    let recipient = ANVIL_ACCOUNT_6.address;

    common::wait_for_balance(&provider, admin.address()).await?;
    common::wait_for_balance(&provider, non_member.address()).await?;

    let client = activated_client(&provider, &admin).await?;

    // --- Create ALLOWLIST policy ---
    let policy_id = create_policy(
        &client,
        admin.address(),
        IPolicyRegistry::PolicyType::ALLOWLIST,
        "createPolicy ALLOWLIST",
    )
    .await?;

    // Non-member is not authorized under the allowlist.
    assert!(
        !is_authorized(&client, policy_id, non_member.address()).await?,
        "non-member must not be authorized on a fresh ALLOWLIST policy",
    );

    // --- Create B20 token and wire the allowlist policy ---
    let token =
        create_token(&client, admin.address(), SALT_ALLOWLIST, "Allowlist Token", "ALT").await?;
    set_transfer_sender_policy(&client, token, policy_id).await?;

    // Seed the non-member with tokens so they have a balance to transfer from.
    client.transfer(token, non_member.address(), U256::from(TRANSFER_AMOUNT)).await?;
    assert_eq!(client.balance_of(token, non_member.address()).await?, U256::from(TRANSFER_AMOUNT));

    // Non-member is not on the allowlist: transfer must revert.
    let non_member_client = B20PrecompileClient::new(&provider, &non_member, common::L2_CHAIN_ID)
        .with_receipt_timeout(common::TX_RECEIPT_TIMEOUT);
    let blocked = non_member_client
        .try_send_call(
            token,
            IB20::transferCall { to: recipient, amount: U256::from(TRANSFER_AMOUNT / 2) },
            "transfer from non-member (should revert)",
        )
        .await?;
    assert!(!blocked, "transfer from non-member must revert when ALLOWLIST policy is wired");

    // --- Add non-member to the allowlist ---
    client
        .send_call(
            PolicyRegistryStorage::ADDRESS,
            IPolicyRegistry::updateAllowlistCall {
                policyId: policy_id,
                allowed: true,
                accounts: vec![non_member.address()],
            },
            "updateAllowlist add non-member",
        )
        .await?;

    // Non-member is now authorized.
    assert!(
        is_authorized(&client, policy_id, non_member.address()).await?,
        "non-member must be authorized after being added to the allowlist",
    );

    // Transfer from the now-allowlisted sender must succeed.
    let allowed = non_member_client
        .try_send_call(
            token,
            IB20::transferCall { to: recipient, amount: U256::from(TRANSFER_AMOUNT / 2) },
            "transfer from allowlisted sender",
        )
        .await?;
    assert!(allowed, "transfer from allowlisted sender must succeed");

    Ok(())
}

/// `test_blocklist_gates_transfer`
///
/// Full cycle:
///   1. Create a BLOCKLIST policy.
///   2. Wire it to the token's `TRANSFER_SENDER_POLICY` slot.
///   3. Assert an unblocked sender can transfer.
///   4. Block the sender.
///   5. Assert their transfer now reverts.
#[tokio::test]
async fn test_blocklist_gates_transfer() -> Result<()> {
    let (_devnet, provider) = common::start_beryl_devnet().await?;

    let admin = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_5.private_key).wrap_err("admin key")?;
    let blocked_sender =
        PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_7.private_key).wrap_err("blocked key")?;
    let recipient = ANVIL_ACCOUNT_6.address;

    common::wait_for_balance(&provider, admin.address()).await?;
    common::wait_for_balance(&provider, blocked_sender.address()).await?;

    let client = activated_client(&provider, &admin).await?;

    // --- Create BLOCKLIST policy ---
    let policy_id = create_policy(
        &client,
        admin.address(),
        IPolicyRegistry::PolicyType::BLOCKLIST,
        "createPolicy BLOCKLIST",
    )
    .await?;

    // The sender is not on the blocklist; they are authorized.
    assert!(
        is_authorized(&client, policy_id, blocked_sender.address()).await?,
        "non-blocked account must be authorized on a fresh BLOCKLIST policy",
    );

    // --- Create B20 token and wire the blocklist policy ---
    let token =
        create_token(&client, admin.address(), SALT_BLOCKLIST, "Blocklist Token", "BLT").await?;
    set_transfer_sender_policy(&client, token, policy_id).await?;

    // Seed the sender with tokens.
    client.transfer(token, blocked_sender.address(), U256::from(TRANSFER_AMOUNT)).await?;
    assert_eq!(
        client.balance_of(token, blocked_sender.address()).await?,
        U256::from(TRANSFER_AMOUNT),
    );

    // Transfer from the (not-yet-blocked) sender must succeed.
    let sender_client = B20PrecompileClient::new(&provider, &blocked_sender, common::L2_CHAIN_ID)
        .with_receipt_timeout(common::TX_RECEIPT_TIMEOUT);
    let first_transfer = sender_client
        .try_send_call(
            token,
            IB20::transferCall { to: recipient, amount: U256::from(TRANSFER_AMOUNT / 2) },
            "transfer from non-blocked sender",
        )
        .await?;
    assert!(first_transfer, "transfer from non-blocked sender must succeed");

    // --- Block the sender ---
    client
        .send_call(
            PolicyRegistryStorage::ADDRESS,
            IPolicyRegistry::updateBlocklistCall {
                policyId: policy_id,
                blocked: true,
                accounts: vec![blocked_sender.address()],
            },
            "updateBlocklist add sender",
        )
        .await?;

    // Sender is now on the blocklist and must not be authorized.
    assert!(
        !is_authorized(&client, policy_id, blocked_sender.address()).await?,
        "blocked account must not be authorized after being added to the blocklist",
    );

    // Transfer from the blocked sender must revert.
    let second_transfer = sender_client
        .try_send_call(
            token,
            IB20::transferCall { to: recipient, amount: U256::from(TRANSFER_AMOUNT / 4) },
            "transfer from blocked sender (should revert)",
        )
        .await?;
    assert!(!second_transfer, "transfer from blocked sender must revert");

    Ok(())
}

/// `test_always_block_policy_blocks_transfer`
///
/// Verifies that the built-in `ALWAYS_BLOCK` policy denies every account via
/// `isAuthorized`, and that wiring it to a token's `TRANSFER_SENDER_POLICY`
/// slot makes ALL transfers revert unconditionally.
#[tokio::test]
async fn test_always_block_policy_blocks_transfer() -> Result<()> {
    let (_devnet, provider) = common::start_beryl_devnet().await?;

    let admin = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_5.private_key).wrap_err("admin key")?;
    let anyone = ANVIL_ACCOUNT_6.address;

    common::wait_for_balance(&provider, admin.address()).await?;

    let client = activated_client(&provider, &admin).await?;

    // ALWAYS_BLOCK must deny every account unconditionally.
    assert!(
        !is_authorized(&client, PolicyRegistryStorage::ALWAYS_BLOCK_ID, admin.address()).await?,
        "ALWAYS_BLOCK must deny the admin",
    );
    assert!(
        !is_authorized(&client, PolicyRegistryStorage::ALWAYS_BLOCK_ID, anyone).await?,
        "ALWAYS_BLOCK must deny any arbitrary account",
    );

    // The ALWAYS_BLOCK policy exists as a built-in.
    let output = client
        .call(
            PolicyRegistryStorage::ADDRESS,
            IPolicyRegistry::policyExistsCall { policyId: PolicyRegistryStorage::ALWAYS_BLOCK_ID },
        )
        .await?;
    let exists = IPolicyRegistry::policyExistsCall::abi_decode_returns(output.as_ref())
        .wrap_err("Failed to decode policyExists")?;
    assert!(exists, "ALWAYS_BLOCK policy must exist");

    // --- Create B20 token and wire ALWAYS_BLOCK to TRANSFER_SENDER_POLICY ---
    let token =
        create_token(&client, admin.address(), SALT_ALWAYS_BLOCK, "Blocked Token", "BLKD").await?;
    set_transfer_sender_policy(&client, token, PolicyRegistryStorage::ALWAYS_BLOCK_ID).await?;

    // Transfer from admin must revert: ALWAYS_BLOCK denies every sender unconditionally.
    let blocked = client
        .try_send_call(
            token,
            IB20::transferCall { to: anyone, amount: U256::from(TRANSFER_AMOUNT) },
            "transfer under ALWAYS_BLOCK (should revert)",
        )
        .await?;
    assert!(!blocked, "transfer from admin must revert under ALWAYS_BLOCK policy");

    Ok(())
}

/// Verifies that a non-admin account cannot call `updateAllowlist` on a policy it does not own.
#[tokio::test]
async fn test_non_admin_cannot_update_policy() -> Result<()> {
    let (_devnet, provider) = common::start_beryl_devnet().await?;

    let admin = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_5.private_key).wrap_err("admin key")?;
    let bob = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_7.private_key).wrap_err("bob key")?;

    common::wait_for_balance(&provider, admin.address()).await?;
    common::wait_for_balance(&provider, bob.address()).await?;

    let client = activated_client(&provider, &admin).await?;

    // Alice (admin) creates the policy.
    let policy_id = create_policy(
        &client,
        admin.address(),
        IPolicyRegistry::PolicyType::ALLOWLIST,
        "createPolicy ALLOWLIST for non-admin update test",
    )
    .await?;

    // Bob is not the policy admin; updateAllowlist must revert with Unauthorized.
    let bob_client = B20PrecompileClient::new(&provider, &bob, common::L2_CHAIN_ID)
        .with_receipt_timeout(common::TX_RECEIPT_TIMEOUT);
    let succeeded = bob_client
        .try_send_call(
            PolicyRegistryStorage::ADDRESS,
            IPolicyRegistry::updateAllowlistCall {
                policyId: policy_id,
                allowed: true,
                accounts: vec![bob.address()],
            },
            "updateAllowlist by non-admin (should revert)",
        )
        .await?;
    assert!(!succeeded, "updateAllowlist by non-admin must revert");

    Ok(())
}

/// Verifies that a non-admin account cannot call `stageUpdateAdmin` on a policy it does not own.
#[tokio::test]
async fn test_non_admin_cannot_stage_admin_transfer() -> Result<()> {
    let (_devnet, provider) = common::start_beryl_devnet().await?;

    let admin = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_5.private_key).wrap_err("admin key")?;
    let bob = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_7.private_key).wrap_err("bob key")?;

    common::wait_for_balance(&provider, admin.address()).await?;
    common::wait_for_balance(&provider, bob.address()).await?;

    let client = activated_client(&provider, &admin).await?;

    // Alice (admin) creates the policy.
    let policy_id = create_policy(
        &client,
        admin.address(),
        IPolicyRegistry::PolicyType::ALLOWLIST,
        "createPolicy ALLOWLIST for non-admin stage-transfer test",
    )
    .await?;

    // Bob tries to nominate himself as the next admin; must revert because Bob does not own the policy.
    let bob_client = B20PrecompileClient::new(&provider, &bob, common::L2_CHAIN_ID)
        .with_receipt_timeout(common::TX_RECEIPT_TIMEOUT);
    let succeeded = bob_client
        .try_send_call(
            PolicyRegistryStorage::ADDRESS,
            IPolicyRegistry::stageUpdateAdminCall { policyId: policy_id, newAdmin: bob.address() },
            "stageUpdateAdmin by non-admin (should revert)",
        )
        .await?;
    assert!(!succeeded, "stageUpdateAdmin by non-admin must revert");

    Ok(())
}

/// Verifies that `updateAllowlist` correctly reflects batch additions and removals via `isAuthorized`.
///
/// Flow:
///   1. Create an ALLOWLIST policy with 5 initial members.
///   2. Add 3 new members in one `updateAllowlist` call.
///   3. Remove 2 of the original members in a second `updateAllowlist` call.
///   4. Assert the 3 added are authorized, the 2 removed are not, and the 3 originals
///      that were not removed are still authorized.
#[tokio::test]
async fn test_policy_membership_correct_after_batch_update() -> Result<()> {
    let (_devnet, provider) = common::start_beryl_devnet().await?;

    let admin = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_5.private_key).wrap_err("admin key")?;
    common::wait_for_balance(&provider, admin.address()).await?;

    let client = activated_client(&provider, &admin).await?;

    // 5 initial members for the allowlist.
    let initial_members = vec![
        ANVIL_ACCOUNT_0.address,
        ANVIL_ACCOUNT_1.address,
        ANVIL_ACCOUNT_2.address,
        ANVIL_ACCOUNT_3.address,
        ANVIL_ACCOUNT_4.address,
    ];

    // Create the ALLOWLIST policy pre-populated with the 5 initial members.
    let create_call = IPolicyRegistry::createPolicyWithAccountsCall {
        admin: admin.address(),
        policyType: IPolicyRegistry::PolicyType::ALLOWLIST,
        accounts: initial_members.clone(),
    };
    let output = client.call(PolicyRegistryStorage::ADDRESS, create_call.clone()).await?;
    let policy_id =
        IPolicyRegistry::createPolicyWithAccountsCall::abi_decode_returns(output.as_ref())
            .wrap_err("Failed to decode createPolicyWithAccounts")?;
    client
        .send_call(PolicyRegistryStorage::ADDRESS, create_call, "createPolicyWithAccounts")
        .await?;

    // All 5 initial members must be authorized immediately after creation.
    for &account in &initial_members {
        assert!(
            is_authorized(&client, policy_id, account).await?,
            "initial member must be authorized right after policy creation",
        );
    }

    // Batch-add 3 new members.
    let added = vec![ANVIL_ACCOUNT_6.address, ANVIL_ACCOUNT_7.address, ANVIL_ACCOUNT_8.address];
    client
        .send_call(
            PolicyRegistryStorage::ADDRESS,
            IPolicyRegistry::updateAllowlistCall {
                policyId: policy_id,
                allowed: true,
                accounts: added.clone(),
            },
            "updateAllowlist add 3 members",
        )
        .await?;

    // Batch-remove 2 of the original members.
    let removed = vec![ANVIL_ACCOUNT_0.address, ANVIL_ACCOUNT_1.address];
    client
        .send_call(
            PolicyRegistryStorage::ADDRESS,
            IPolicyRegistry::updateAllowlistCall {
                policyId: policy_id,
                allowed: false,
                accounts: removed.clone(),
            },
            "updateAllowlist remove 2 members",
        )
        .await?;

    // The 2 removed members must no longer be authorized.
    for &account in &removed {
        assert!(
            !is_authorized(&client, policy_id, account).await?,
            "removed member must not be authorized after removal",
        );
    }

    // The 3 original members that were NOT removed must still be authorized.
    let retained = [ANVIL_ACCOUNT_2.address, ANVIL_ACCOUNT_3.address, ANVIL_ACCOUNT_4.address];
    for &account in &retained {
        assert!(
            is_authorized(&client, policy_id, account).await?,
            "retained member must still be authorized",
        );
    }

    // The 3 newly added members must now be authorized.
    for &account in &added {
        assert!(
            is_authorized(&client, policy_id, account).await?,
            "added member must be authorized after batch add",
        );
    }

    Ok(())
}

/// Verifies that allowlist policy state persists across several empty blocks.
///
/// Flow:
///   1. Create an ALLOWLIST policy and wire it to TRANSFER_SENDER_POLICY.
///   2. Add alice to the allowlist.
///   3. Wait for several blocks to be produced (state persists across them).
///   4. Assert alice can still transfer after the gap.
#[tokio::test]
async fn test_policy_authorization_persists_across_blocks() -> Result<()> {
    let (_devnet, provider) = common::start_beryl_devnet().await?;

    let admin = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_5.private_key).wrap_err("admin key")?;
    let alice = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_6.private_key).wrap_err("alice key")?;
    let recipient = ANVIL_ACCOUNT_7.address;

    common::wait_for_balance(&provider, admin.address()).await?;
    common::wait_for_balance(&provider, alice.address()).await?;

    let client = activated_client(&provider, &admin).await?;

    // --- Create ALLOWLIST policy and add alice immediately ---
    let policy_id = create_policy(
        &client,
        admin.address(),
        IPolicyRegistry::PolicyType::ALLOWLIST,
        "createPolicy ALLOWLIST persist-across-blocks",
    )
    .await?;

    client
        .send_call(
            PolicyRegistryStorage::ADDRESS,
            IPolicyRegistry::updateAllowlistCall {
                policyId: policy_id,
                allowed: true,
                accounts: vec![alice.address()],
            },
            "updateAllowlist add alice",
        )
        .await?;

    // --- Create B20 token and wire the allowlist policy ---
    let token =
        create_token(&client, admin.address(), SALT_PERSIST_BLOCKS, "Persist Token", "PERS")
            .await?;
    set_transfer_sender_policy(&client, token, policy_id).await?;

    // Seed alice with tokens.
    client.transfer(token, alice.address(), U256::from(TRANSFER_AMOUNT)).await?;

    // Wait for several more blocks to be produced after the policy setup.
    let current_block = common::wait_for_block(&provider, 0).await?;
    common::wait_for_block(&provider, current_block + 3).await?;

    // Alice should still be authorized after the block gap.
    assert!(
        is_authorized(&client, policy_id, alice.address()).await?,
        "alice must remain authorized after several empty blocks",
    );

    // Alice's transfer must still succeed after the gap.
    let alice_client = B20PrecompileClient::new(&provider, &alice, common::L2_CHAIN_ID)
        .with_receipt_timeout(common::TX_RECEIPT_TIMEOUT);
    let succeeded = alice_client
        .try_send_call(
            token,
            IB20::transferCall { to: recipient, amount: U256::from(TRANSFER_AMOUNT / 2) },
            "transfer from alice after block gap",
        )
        .await?;
    assert!(succeeded, "alice's transfer must succeed after state persists across blocks");

    Ok(())
}

/// Verifies that a B20 token with no policy set uses ALWAYS_ALLOW (policy id 0) by default.
///
/// Flow:
///   1. Create a B20 token but do NOT call `updatePolicy`.
///   2. Assert that alice can transfer freely (ALWAYS_ALLOW_ID = 0 is the default for
///      uninitialized policy slots).
#[tokio::test]
async fn test_always_allow_is_default_before_any_policy_set() -> Result<()> {
    let (_devnet, provider) = common::start_beryl_devnet().await?;

    let admin = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_5.private_key).wrap_err("admin key")?;
    let alice = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_6.private_key).wrap_err("alice key")?;
    let recipient = ANVIL_ACCOUNT_7.address;

    common::wait_for_balance(&provider, admin.address()).await?;
    common::wait_for_balance(&provider, alice.address()).await?;

    let client = activated_client(&provider, &admin).await?;

    // --- Create B20 token WITHOUT calling updatePolicy ---
    let token =
        create_token(&client, admin.address(), SALT_DEFAULT_ALLOW, "Default Token", "DFLT").await?;

    // Seed alice with tokens so she has a balance to transfer from.
    client.transfer(token, alice.address(), U256::from(TRANSFER_AMOUNT)).await?;
    assert_eq!(client.balance_of(token, alice.address()).await?, U256::from(TRANSFER_AMOUNT));

    // No policy has been set: the default TRANSFER_SENDER_POLICY is ALWAYS_ALLOW_ID (0).
    // Alice must be able to transfer without any restriction.
    let alice_client = B20PrecompileClient::new(&provider, &alice, common::L2_CHAIN_ID)
        .with_receipt_timeout(common::TX_RECEIPT_TIMEOUT);
    let succeeded = alice_client
        .try_send_call(
            token,
            IB20::transferCall { to: recipient, amount: U256::from(TRANSFER_AMOUNT / 2) },
            "transfer from alice with default ALWAYS_ALLOW policy",
        )
        .await?;
    assert!(succeeded, "alice's transfer must succeed under default ALWAYS_ALLOW policy");

    Ok(())
}
