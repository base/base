//! End-to-end tests for the activation registry precompile over Base node RPC.

mod common;

use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::SolCall;
use base_common_precompiles::{ActivationRegistryStorage, IActivationRegistry};
use devnet::{B20PrecompileClient, config::ANVIL_ACCOUNT_5};
use eyre::{Result, WrapErr};

/// `isActivated` returns `false` for every feature id by default.
#[tokio::test]
async fn test_activation_registry_is_activated_default() -> Result<()> {
    let (_devnet, provider) = common::start_beryl_devnet().await?;
    let admin = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_5.private_key)
        .wrap_err("Failed to parse devnet private key")?;
    common::wait_for_balance(&provider, admin.address()).await?;

    let client = B20PrecompileClient::new(&provider, &admin, common::L2_CHAIN_ID)
        .with_receipt_timeout(common::TX_RECEIPT_TIMEOUT);

    let output = client
        .call(
            ActivationRegistryStorage::ADDRESS,
            IActivationRegistry::isActivatedCall {
                feature: ActivationRegistryStorage::SECURITIES_TOKEN_CREATION,
            },
        )
        .await?;
    let is_activated = IActivationRegistry::isActivatedCall::abi_decode_returns(output.as_ref())
        .wrap_err("Failed to decode isActivated")?;

    assert!(!is_activated, "feature should be inactive by default");

    Ok(())
}

/// `admin()` returns the hardcoded activation admin address.
#[tokio::test]
async fn test_activation_registry_admin() -> Result<()> {
    let (_devnet, provider) = common::start_beryl_devnet().await?;
    let caller = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_5.private_key)
        .wrap_err("Failed to parse devnet private key")?;
    common::wait_for_balance(&provider, caller.address()).await?;

    let client = B20PrecompileClient::new(&provider, &caller, common::L2_CHAIN_ID)
        .with_receipt_timeout(common::TX_RECEIPT_TIMEOUT);

    let output =
        client.call(ActivationRegistryStorage::ADDRESS, IActivationRegistry::adminCall {}).await?;
    let admin_addr = IActivationRegistry::adminCall::abi_decode_returns(output.as_ref())
        .wrap_err("Failed to decode admin")?;

    assert_eq!(admin_addr, ActivationRegistryStorage::ADMIN);

    Ok(())
}

/// Calling `activate` from a non-admin account reverts with `Unauthorized`.
#[tokio::test]
async fn test_activation_registry_unauthorized_activate_reverts() -> Result<()> {
    let (_devnet, provider) = common::start_beryl_devnet().await?;
    let non_admin = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_5.private_key)
        .wrap_err("Failed to parse devnet private key")?;
    common::wait_for_balance(&provider, non_admin.address()).await?;

    let client = B20PrecompileClient::new(&provider, &non_admin, common::L2_CHAIN_ID)
        .with_receipt_timeout(common::TX_RECEIPT_TIMEOUT);

    let succeeded = client
        .try_send_call(
            ActivationRegistryStorage::ADDRESS,
            IActivationRegistry::activateCall {
                feature: ActivationRegistryStorage::SECURITIES_TOKEN_CREATION,
            },
            "activate (unauthorized)",
        )
        .await?;

    assert!(!succeeded, "activate from non-admin should revert");

    // Feature remains inactive after the failed attempt.
    let output = client
        .call(
            ActivationRegistryStorage::ADDRESS,
            IActivationRegistry::isActivatedCall {
                feature: ActivationRegistryStorage::SECURITIES_TOKEN_CREATION,
            },
        )
        .await?;
    let is_activated = IActivationRegistry::isActivatedCall::abi_decode_returns(output.as_ref())
        .wrap_err("Failed to decode isActivated")?;
    assert!(!is_activated, "feature should still be inactive after unauthorized activate");

    Ok(())
}
