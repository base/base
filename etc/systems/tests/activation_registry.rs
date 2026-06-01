//! System tests for the activation registry precompile over Base node RPC.

mod common;

use alloy_primitives::{Address, B256, LogData};
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::{SolCall, SolEvent};
use base_common_precompiles::{ActivationFeature, ActivationRegistryStorage, IActivationRegistry};
use base_common_rpc_types::BaseTransactionReceipt;
use base_system_tests::{ANVIL_ACCOUNT_5, ANVIL_ACCOUNT_6, B20PrecompileClient};
use eyre::{Result, WrapErr};

/// `isActivated` returns `false` for every feature id by default.
#[tokio::test]
async fn test_activation_registry_is_activated_default() -> Result<()> {
    let (_system, provider) = common::start_beryl_system().await?;
    let admin = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_5.private_key)
        .wrap_err("Failed to parse system test private key")?;
    common::wait_for_balance(&provider, admin.address()).await?;

    let client = B20PrecompileClient::new(&provider, &admin, common::L2_CHAIN_ID)
        .with_receipt_timeout(common::TX_RECEIPT_TIMEOUT);

    let output = client
        .call(
            ActivationRegistryStorage::ADDRESS,
            IActivationRegistry::isActivatedCall { feature: ActivationFeature::B20Asset.id() },
        )
        .await?;
    let is_activated = IActivationRegistry::isActivatedCall::abi_decode_returns(output.as_ref())
        .wrap_err("Failed to decode isActivated")?;

    assert!(!is_activated, "feature should be inactive by default");

    Ok(())
}

/// `admin()` returns the generated system test activation admin address.
#[tokio::test]
async fn test_activation_registry_admin() -> Result<()> {
    let (_system, provider) = common::start_beryl_system().await?;
    let caller = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_5.private_key)
        .wrap_err("Failed to parse system test private key")?;
    common::wait_for_balance(&provider, caller.address()).await?;

    let client = B20PrecompileClient::new(&provider, &caller, common::L2_CHAIN_ID)
        .with_receipt_timeout(common::TX_RECEIPT_TIMEOUT);

    let output =
        client.call(ActivationRegistryStorage::ADDRESS, IActivationRegistry::adminCall {}).await?;
    let admin_addr = IActivationRegistry::adminCall::abi_decode_returns(output.as_ref())
        .wrap_err("Failed to decode admin")?;

    assert_eq!(admin_addr, ANVIL_ACCOUNT_5.address);

    Ok(())
}

/// Admin activate/deactivate transitions update state, emit events, and reject repeated calls.
#[tokio::test]
async fn test_activation_registry_admin_lifecycle() -> Result<()> {
    let (_system, provider) = common::start_beryl_system().await?;
    let admin = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_5.private_key)
        .wrap_err("Failed to parse system test private key")?;
    common::wait_for_balance(&provider, admin.address()).await?;

    let client = B20PrecompileClient::new(&provider, &admin, common::L2_CHAIN_ID)
        .with_receipt_timeout(common::TX_RECEIPT_TIMEOUT);
    let feature = ActivationFeature::B20Stablecoin.id();

    assert!(!is_activated(&client, feature).await?, "feature should start inactive");

    let activate_receipt = client
        .send_call_receipt(
            ActivationRegistryStorage::ADDRESS,
            IActivationRegistry::activateCall { feature },
            "activate feature",
        )
        .await?;
    assert_activation_log(&activate_receipt, feature, admin.address(), true);
    assert!(is_activated(&client, feature).await?, "feature should be active after activate");

    let activate_again = client
        .try_send_call(
            ActivationRegistryStorage::ADDRESS,
            IActivationRegistry::activateCall { feature },
            "activate feature again",
        )
        .await?;
    assert!(!activate_again, "repeated activate should revert");
    assert!(is_activated(&client, feature).await?, "feature should remain active");

    let deactivate_receipt = client
        .send_call_receipt(
            ActivationRegistryStorage::ADDRESS,
            IActivationRegistry::deactivateCall { feature },
            "deactivate feature",
        )
        .await?;
    assert_activation_log(&deactivate_receipt, feature, admin.address(), false);
    assert!(!is_activated(&client, feature).await?, "feature should be inactive after deactivate");

    let deactivate_again = client
        .try_send_call(
            ActivationRegistryStorage::ADDRESS,
            IActivationRegistry::deactivateCall { feature },
            "deactivate feature again",
        )
        .await?;
    assert!(!deactivate_again, "repeated deactivate should revert");
    assert!(!is_activated(&client, feature).await?, "feature should remain inactive");

    let reactivate_receipt = client
        .send_call_receipt(
            ActivationRegistryStorage::ADDRESS,
            IActivationRegistry::activateCall { feature },
            "reactivate feature",
        )
        .await?;
    assert_activation_log(&reactivate_receipt, feature, admin.address(), true);
    assert!(is_activated(&client, feature).await?, "feature should be active after reactivation");

    Ok(())
}

/// Calling `activate` from a non-admin account reverts with `Unauthorized`.
#[tokio::test]
async fn test_activation_registry_unauthorized_activate_reverts() -> Result<()> {
    let (_system, provider) = common::start_beryl_system().await?;
    let non_admin = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_6.private_key)
        .wrap_err("Failed to parse system test private key")?;
    common::wait_for_balance(&provider, non_admin.address()).await?;

    let client = B20PrecompileClient::new(&provider, &non_admin, common::L2_CHAIN_ID)
        .with_receipt_timeout(common::TX_RECEIPT_TIMEOUT);

    let succeeded = client
        .try_send_call(
            ActivationRegistryStorage::ADDRESS,
            IActivationRegistry::activateCall { feature: ActivationFeature::B20Asset.id() },
            "activate (unauthorized)",
        )
        .await?;

    assert!(!succeeded, "activate from non-admin should revert");

    // Feature remains inactive after the failed attempt.
    let output = client
        .call(
            ActivationRegistryStorage::ADDRESS,
            IActivationRegistry::isActivatedCall { feature: ActivationFeature::B20Asset.id() },
        )
        .await?;
    let is_activated = IActivationRegistry::isActivatedCall::abi_decode_returns(output.as_ref())
        .wrap_err("Failed to decode isActivated")?;
    assert!(!is_activated, "feature should still be inactive after unauthorized activate");

    Ok(())
}

async fn is_activated(client: &B20PrecompileClient<'_>, feature: B256) -> Result<bool> {
    let output = client
        .call(ActivationRegistryStorage::ADDRESS, IActivationRegistry::isActivatedCall { feature })
        .await?;
    IActivationRegistry::isActivatedCall::abi_decode_returns(output.as_ref())
        .wrap_err("Failed to decode isActivated")
}

fn assert_activation_log(
    receipt: &BaseTransactionReceipt,
    feature: B256,
    caller: Address,
    active: bool,
) {
    let expected = if active {
        IActivationRegistry::FeatureActivated { feature, caller }.encode_log_data()
    } else {
        IActivationRegistry::FeatureDeactivated { feature, caller }.encode_log_data()
    };
    assert_receipt_log(receipt, ActivationRegistryStorage::ADDRESS, expected);
}

fn assert_receipt_log(receipt: &BaseTransactionReceipt, address: Address, expected: LogData) {
    assert!(
        receipt.inner.logs().iter().any(|log| log.address() == address && log.data() == &expected),
        "receipt must contain expected log at {address}; expected={expected:?}, logs={:?}",
        receipt.inner.logs()
    );
}
