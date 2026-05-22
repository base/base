//! End-to-end tests for policy-gated B20 token transfers over Base node RPC.
//!
//! The policy registry precompile is live on Beryl, but B20 tokens do not yet
//! expose a `setPolicy` method to wire a named policy to transfer enforcement.
//! Until that wiring lands, tokens always transfer freely regardless of policy
//! membership (the `ALWAYS_ALLOW` built-in is effectively in effect for every
//! transfer).
//!
//! Each test below:
//!   - Exercises the policy registry over real RPC (create, membership, auth
//!     queries).
//!   - Creates a B20 token and demonstrates that transfers succeed regardless
//!     of the associated policy state.
//!   - Includes a TODO marking where the full gating assertion belongs once
//!     `setPolicy` (or an equivalent init-call hook) lands on `IB20`.

mod common;

use alloy_primitives::{Address, B256, U256};
use alloy_provider::RootProvider;
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::SolCall;
use base_common_network::Base;
use base_common_precompiles::{
    ActivationFeature, IB20, IPolicyRegistry, PolicyRegistryStorage, TokenVariant,
};
use devnet::{
    B20PrecompileClient,
    config::{ANVIL_ACCOUNT_5, ANVIL_ACCOUNT_6, ANVIL_ACCOUNT_7},
};
use eyre::{Result, WrapErr};

const INITIAL_SUPPLY: u64 = 1_000_000;
const TRANSFER_AMOUNT: u64 = 100_000;

// Salts must not overlap with those used in b20_precompile.rs (0x10–0x18, 0x42).
const SALT_ALLOWLIST: B256 = B256::repeat_byte(0x50);
const SALT_BLOCKLIST: B256 = B256::repeat_byte(0x51);
const SALT_ALWAYS_BLOCK: B256 = B256::repeat_byte(0x52);

/// Activates `TOKEN_FACTORY`, `B20_TOKEN`, and `POLICY_REGISTRY` features, then
/// returns a [`B20PrecompileClient`] ready for precompile calls.
async fn activated_client<'a>(
    provider: &'a RootProvider<Base>,
    admin: &'a PrivateKeySigner,
) -> Result<B20PrecompileClient<'a>> {
    let client = B20PrecompileClient::new(provider, admin, common::L2_CHAIN_ID)
        .with_receipt_timeout(common::TX_RECEIPT_TIMEOUT);
    client.activate_feature(ActivationFeature::TokenFactory.id()).await?;
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
    let output = client.call(PolicyRegistryStorage::ADDRESS, call).await?;
    let policy_id = IPolicyRegistry::createPolicyCall::abi_decode_returns(output.as_ref())
        .wrap_err("Failed to decode createPolicy return")?;
    client
        .send_call(
            PolicyRegistryStorage::ADDRESS,
            IPolicyRegistry::createPolicyCall { admin, policyType: policy_type },
            label,
        )
        .await?;
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
    let token = client.create_token(TokenVariant::B20, params, salt).await?;
    client
        .wait_for_token_code(token, common::TX_RECEIPT_TIMEOUT, common::BLOCK_POLL_INTERVAL)
        .await?;
    Ok(token)
}

/// `test_allowlist_gates_transfer`
///
/// Verifies the ALLOWLIST policy mechanics over RPC:
///   - A non-member is not authorized.
///   - After `updateAllowlist`, the member is authorized.
///
/// Also demonstrates that B20 transfers currently succeed for any sender
/// because `IB20` does not yet expose `setPolicy`.
///
/// TODO(BOP-125): Once `setPolicy` (or an init-call hook) lands on `IB20`,
/// extend this test to:
///   1. Wire the ALLOWLIST policy to the token via `setPolicy`.
///   2. Assert that a transfer FROM the non-member reverts.
///   3. Assert that the same transfer succeeds after `updateAllowlist`.
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

    // --- Create B20 token ---
    let token =
        create_token(&client, admin.address(), SALT_ALLOWLIST, "Allowlist Token", "ALT").await?;

    // Seed the non-member with tokens so they have a balance to transfer from.
    client.transfer(token, non_member.address(), U256::from(TRANSFER_AMOUNT)).await?;
    assert_eq!(client.balance_of(token, non_member.address()).await?, U256::from(TRANSFER_AMOUNT),);

    // Transfer from non-member succeeds today because IB20 has no setPolicy.
    // TODO(BOP-125): This should revert once the allowlist is wired to the token.
    let non_member_client = B20PrecompileClient::new(&provider, &non_member, common::L2_CHAIN_ID)
        .with_receipt_timeout(common::TX_RECEIPT_TIMEOUT);
    let transfer_succeeded = non_member_client
        .try_send_call(
            token,
            IB20::transferCall { to: recipient, amount: U256::from(TRANSFER_AMOUNT / 2) },
            "transfer from non-member",
        )
        .await?;
    assert!(transfer_succeeded, "transfer from non-member must succeed (no policy wiring yet)");

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

    // TODO(BOP-125): Once setPolicy exists, assert the transfer from the
    // newly-allowlisted sender succeeds.

    Ok(())
}

/// `test_blocklist_gates_transfer`
///
/// Verifies the BLOCKLIST policy mechanics over RPC:
///   - A non-blocked account is authorized.
///   - After `updateBlocklist`, the blocked account is not authorized.
///
/// Also demonstrates that B20 transfers currently succeed for any sender,
/// including one on the blocklist, because `IB20` does not yet expose
/// `setPolicy`.
///
/// TODO(BOP-125): Once `setPolicy` (or an init-call hook) lands on `IB20`,
/// extend this test to:
///   1. Wire the BLOCKLIST policy to the token via `setPolicy`.
///   2. Assert that a transfer FROM the blocked sender reverts.
///   3. Assert that the same transfer succeeds after `updateBlocklist` removes
///      the sender from the blocklist.
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

    // --- Create B20 token ---
    let token =
        create_token(&client, admin.address(), SALT_BLOCKLIST, "Blocklist Token", "BLT").await?;

    // Seed the sender with tokens.
    client.transfer(token, blocked_sender.address(), U256::from(TRANSFER_AMOUNT)).await?;
    assert_eq!(
        client.balance_of(token, blocked_sender.address()).await?,
        U256::from(TRANSFER_AMOUNT),
    );

    // Transfer from the (not-yet-blocked) sender succeeds.
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

    // Transfer still succeeds today because IB20 has no setPolicy.
    // TODO(BOP-125): This should revert once the blocklist is wired to the token.
    let second_transfer = sender_client
        .try_send_call(
            token,
            IB20::transferCall { to: recipient, amount: U256::from(TRANSFER_AMOUNT / 4) },
            "transfer from blocked sender",
        )
        .await?;
    assert!(
        second_transfer,
        "transfer from blocked sender must still succeed (no policy wiring yet)",
    );

    Ok(())
}

/// `test_always_block_policy_blocks_transfer`
///
/// Verifies that the built-in `ALWAYS_BLOCK` policy (id = 1) denies every
/// account via `isAuthorized`.
///
/// Also demonstrates that B20 transfers currently succeed even when the token
/// is conceptually under an `ALWAYS_BLOCK` policy, because `IB20` does not yet
/// expose `setPolicy`.
///
/// TODO(BOP-125): Once `setPolicy` (or an init-call hook) lands on `IB20`,
/// extend this test to:
///   1. Wire `ALWAYS_BLOCK_ID` to the token via `setPolicy`.
///   2. Assert that ALL transfers from ANY address revert.
#[tokio::test]
async fn test_always_block_policy_blocks_transfer() -> Result<()> {
    let (_devnet, provider) = common::start_beryl_devnet().await?;

    let admin = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_5.private_key).wrap_err("admin key")?;
    let anyone = ANVIL_ACCOUNT_6.address;

    common::wait_for_balance(&provider, admin.address()).await?;

    let client = activated_client(&provider, &admin).await?;

    // ALWAYS_BLOCK (id = 1) must deny every account unconditionally.
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

    // --- Create B20 token ---
    let token =
        create_token(&client, admin.address(), SALT_ALWAYS_BLOCK, "Blocked Token", "BLKD").await?;

    // Transfer from admin succeeds today because IB20 has no setPolicy.
    // TODO(BOP-125): Once setPolicy exists, configure the token with
    // ALWAYS_BLOCK_ID and assert ALL transfers revert.
    let transfer_succeeded = client
        .try_send_call(
            token,
            IB20::transferCall { to: anyone, amount: U256::from(TRANSFER_AMOUNT) },
            "transfer under ALWAYS_BLOCK (no wiring yet)",
        )
        .await?;
    assert!(transfer_succeeded, "transfer must succeed today (ALWAYS_BLOCK not yet wired to IB20)",);

    Ok(())
}
