//! System tests for the activation registry precompile over Base node RPC.

mod common;

use alloy_primitives::{Address, B256, LogData};
use alloy_provider::RootProvider;
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::{SolCall, SolEvent};
use base_common_network::Base;
use base_common_precompiles::{ActivationFeature, ActivationRegistryStorage, IActivationRegistry};
use base_common_rpc_types::BaseTransactionReceipt;
use base_system_tests::{
    ANVIL_ACCOUNT_5, ANVIL_ACCOUNT_6, B20PrecompileClient, SystemTestStack, SystemTestStackBuilder,
};
use eyre::{Result, WrapErr};

const BASE_COBALT_ACTIVATION_BLOCK: u64 = 5;

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

/// `setAdmin` stays disabled while Beryl is active but Cobalt is not.
#[tokio::test]
async fn test_activation_registry_set_admin_reverts_before_cobalt() -> Result<()> {
    let (_system, provider) = common::start_beryl_system().await?;
    let admin = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_5.private_key)
        .wrap_err("Failed to parse system test private key")?;
    common::wait_for_balance(&provider, admin.address()).await?;

    let client = B20PrecompileClient::new(&provider, &admin, common::L2_CHAIN_ID)
        .with_receipt_timeout(common::TX_RECEIPT_TIMEOUT);

    let succeeded = client
        .try_send_call(
            ActivationRegistryStorage::ADDRESS,
            IActivationRegistry::setAdminCall { newAdmin: ANVIL_ACCOUNT_6.address },
            "set activation admin before Cobalt",
        )
        .await?;

    assert!(!succeeded, "setAdmin should revert before Cobalt");
    assert_eq!(admin_address(&client).await?, ANVIL_ACCOUNT_5.address);

    Ok(())
}

/// At Cobalt, `setAdmin` updates the stored admin and future activation authority.
#[tokio::test]
async fn test_activation_registry_cobalt_admin_rotation() -> Result<()> {
    let (_system, provider) = start_cobalt_system().await?;
    let admin = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_5.private_key)
        .wrap_err("Failed to parse system test admin private key")?;
    let new_admin = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_6.private_key)
        .wrap_err("Failed to parse system test new admin private key")?;
    common::wait_for_balance(&provider, admin.address()).await?;
    common::wait_for_balance(&provider, new_admin.address()).await?;

    let admin_client = B20PrecompileClient::new(&provider, &admin, common::L2_CHAIN_ID)
        .with_receipt_timeout(common::TX_RECEIPT_TIMEOUT);
    let new_admin_client = B20PrecompileClient::new(&provider, &new_admin, common::L2_CHAIN_ID)
        .with_receipt_timeout(common::TX_RECEIPT_TIMEOUT);

    assert_eq!(admin_address(&admin_client).await?, admin.address());

    let set_admin_receipt = admin_client
        .send_call_receipt(
            ActivationRegistryStorage::ADDRESS,
            IActivationRegistry::setAdminCall { newAdmin: new_admin.address() },
            "set activation admin",
        )
        .await?;
    assert_admin_changed_log(
        &set_admin_receipt,
        admin.address(),
        new_admin.address(),
        admin.address(),
    );
    assert_eq!(admin_address(&admin_client).await?, new_admin.address());

    let feature = ActivationFeature::B20Stablecoin.id();
    let old_admin_activate = admin_client
        .try_send_call(
            ActivationRegistryStorage::ADDRESS,
            IActivationRegistry::activateCall { feature },
            "activate feature with old admin",
        )
        .await?;
    assert!(!old_admin_activate, "previous admin should not activate after rotation");

    let new_admin_activate = new_admin_client
        .send_call_receipt(
            ActivationRegistryStorage::ADDRESS,
            IActivationRegistry::activateCall { feature },
            "activate feature with new admin",
        )
        .await?;
    assert_activation_log(&new_admin_activate, feature, new_admin.address(), true);
    assert!(is_activated(&new_admin_client, feature).await?, "new admin activation should persist");

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

/// `checkActivated` reverts before activation and succeeds while the feature is active.
#[tokio::test]
async fn test_activation_registry_check_activated_gate() -> Result<()> {
    let (_system, provider) = common::start_beryl_system().await?;
    let admin = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_5.private_key)
        .wrap_err("Failed to parse system test private key")?;
    common::wait_for_balance(&provider, admin.address()).await?;

    let client = B20PrecompileClient::new(&provider, &admin, common::L2_CHAIN_ID)
        .with_receipt_timeout(common::TX_RECEIPT_TIMEOUT);
    let feature = ActivationFeature::B20Asset.id();

    assert!(
        client
            .call(
                ActivationRegistryStorage::ADDRESS,
                IActivationRegistry::checkActivatedCall { feature },
            )
            .await
            .is_err(),
        "checkActivated should revert while feature is inactive"
    );

    client.activate_feature(feature).await?;
    assert!(is_activated(&client, feature).await?, "feature should be active");
    client
        .call(
            ActivationRegistryStorage::ADDRESS,
            IActivationRegistry::checkActivatedCall { feature },
        )
        .await
        .wrap_err("checkActivated should succeed while feature is active")?;

    client.deactivate_feature(feature).await?;
    assert!(!is_activated(&client, feature).await?, "feature should be inactive");

    Ok(())
}

async fn is_activated(client: &B20PrecompileClient<'_>, feature: B256) -> Result<bool> {
    let output = client
        .call(ActivationRegistryStorage::ADDRESS, IActivationRegistry::isActivatedCall { feature })
        .await?;
    IActivationRegistry::isActivatedCall::abi_decode_returns(output.as_ref())
        .wrap_err("Failed to decode isActivated")
}

async fn start_cobalt_system() -> Result<(SystemTestStack, RootProvider<Base>)> {
    let system = SystemTestStackBuilder::new()
        .with_l1_chain_id(common::L1_CHAIN_ID)
        .with_l2_chain_id(common::L2_CHAIN_ID)
        .with_base_azul_activation_block(common::BASE_AZUL_ACTIVATION_BLOCK)
        .with_base_beryl_activation_block(common::BASE_BERYL_ACTIVATION_BLOCK)
        .with_base_cobalt_activation_block(BASE_COBALT_ACTIVATION_BLOCK)
        .build()
        .await?;
    let provider = system.l2_builder_provider()?;
    common::wait_for_block(&provider, BASE_COBALT_ACTIVATION_BLOCK + 1).await?;
    Ok((system, provider))
}

async fn admin_address(client: &B20PrecompileClient<'_>) -> Result<Address> {
    let output =
        client.call(ActivationRegistryStorage::ADDRESS, IActivationRegistry::adminCall {}).await?;
    IActivationRegistry::adminCall::abi_decode_returns(output.as_ref())
        .wrap_err("Failed to decode admin")
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

fn assert_admin_changed_log(
    receipt: &BaseTransactionReceipt,
    previous_admin: Address,
    new_admin: Address,
    caller: Address,
) {
    let expected = IActivationRegistry::AdminChanged {
        previousAdmin: previous_admin,
        newAdmin: new_admin,
        caller,
    }
    .encode_log_data();
    assert_receipt_log(receipt, ActivationRegistryStorage::ADDRESS, expected);
}

fn assert_receipt_log(receipt: &BaseTransactionReceipt, address: Address, expected: LogData) {
    assert!(
        receipt.inner.logs().iter().any(|log| log.address() == address && log.data() == &expected),
        "receipt must contain expected log at {address}; expected={expected:?}, logs={:?}",
        receipt.inner.logs()
    );
}
