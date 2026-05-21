//! End-to-end tests for the policy registry precompile over Base node RPC.

mod common;

use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::SolCall;
use base_common_precompiles::{IPolicyRegistry, PolicyRegistryStorage};
use devnet::{B20PrecompileClient, config::ANVIL_ACCOUNT_5};
use eyre::{Result, WrapErr};

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
