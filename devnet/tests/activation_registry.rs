//! End-to-end tests for the activation registry precompile over Base node RPC.

use std::time::Duration;

use alloy_primitives::U256;
use alloy_provider::{Provider, RootProvider};
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::SolCall;
use base_common_network::Base;
use base_common_precompiles::{ActivationRegistry, IActivationRegistry};
use devnet::{
    B20PrecompileClient, Devnet, DevnetBuilder,
    config::ANVIL_ACCOUNT_5,
};
use eyre::{Result, WrapErr};
use tokio::time::{sleep, timeout};

const L1_CHAIN_ID: u64 = 1337;
const L2_CHAIN_ID: u64 = 84538453;
const BASE_AZUL_ACTIVATION_BLOCK: u64 = 0;
const BASE_BERYL_ACTIVATION_BLOCK: u64 = 3;
const BLOCK_PRODUCTION_TIMEOUT: Duration = Duration::from_secs(30);
const BLOCK_POLL_INTERVAL: Duration = Duration::from_millis(500);
const TX_RECEIPT_TIMEOUT: Duration = Duration::from_secs(60);

/// `isActivated` returns `false` for every feature id by default.
#[tokio::test]
async fn test_activation_registry_is_activated_default() -> Result<()> {
    let devnet = ActivationDevnet::start().await?;
    let admin = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_5.private_key)
        .wrap_err("Failed to parse devnet private key")?;
    devnet.wait_for_balance(admin.address()).await?;

    let client = B20PrecompileClient::new(devnet.provider(), &admin, L2_CHAIN_ID)
        .with_receipt_timeout(TX_RECEIPT_TIMEOUT);

    let output = client
        .call(
            ActivationRegistry::ADDRESS,
            IActivationRegistry::isActivatedCall {
                feature: ActivationRegistry::SECURITIES_TOKEN_CREATION,
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
    let devnet = ActivationDevnet::start().await?;
    let caller = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_5.private_key)
        .wrap_err("Failed to parse devnet private key")?;
    devnet.wait_for_balance(caller.address()).await?;

    let client = B20PrecompileClient::new(devnet.provider(), &caller, L2_CHAIN_ID)
        .with_receipt_timeout(TX_RECEIPT_TIMEOUT);

    let output =
        client.call(ActivationRegistry::ADDRESS, IActivationRegistry::adminCall {}).await?;
    let admin_addr = IActivationRegistry::adminCall::abi_decode_returns(output.as_ref())
        .wrap_err("Failed to decode admin")?;

    assert_eq!(admin_addr, ActivationRegistry::ADMIN);

    Ok(())
}

/// Calling `activate` from a non-admin account reverts with `Unauthorized`.
#[tokio::test]
async fn test_activation_registry_unauthorized_activate_reverts() -> Result<()> {
    let devnet = ActivationDevnet::start().await?;
    let non_admin = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_5.private_key)
        .wrap_err("Failed to parse devnet private key")?;
    devnet.wait_for_balance(non_admin.address()).await?;

    let client = B20PrecompileClient::new(devnet.provider(), &non_admin, L2_CHAIN_ID)
        .with_receipt_timeout(TX_RECEIPT_TIMEOUT);

    let succeeded = client
        .try_send_call(
            ActivationRegistry::ADDRESS,
            IActivationRegistry::activateCall {
                feature: ActivationRegistry::SECURITIES_TOKEN_CREATION,
            },
        )
        .await?;

    assert!(!succeeded, "activate from non-admin should revert");

    // Feature remains inactive after the failed attempt.
    let output = client
        .call(
            ActivationRegistry::ADDRESS,
            IActivationRegistry::isActivatedCall {
                feature: ActivationRegistry::SECURITIES_TOKEN_CREATION,
            },
        )
        .await?;
    let is_activated = IActivationRegistry::isActivatedCall::abi_decode_returns(output.as_ref())
        .wrap_err("Failed to decode isActivated")?;
    assert!(!is_activated, "feature should still be inactive after unauthorized activate");

    Ok(())
}

struct ActivationDevnet {
    _devnet: Devnet,
    provider: RootProvider<Base>,
}

impl ActivationDevnet {
    async fn start() -> Result<Self> {
        let devnet = DevnetBuilder::new()
            .with_l1_chain_id(L1_CHAIN_ID)
            .with_l2_chain_id(L2_CHAIN_ID)
            .with_base_azul_activation_block(BASE_AZUL_ACTIVATION_BLOCK)
            .with_base_beryl_activation_block(BASE_BERYL_ACTIVATION_BLOCK)
            .build()
            .await?;

        let provider = devnet.l2_builder_provider()?;
        let this = Self { _devnet: devnet, provider };
        this.wait_for_block(BASE_BERYL_ACTIVATION_BLOCK + 1).await?;
        Ok(this)
    }

    const fn provider(&self) -> &RootProvider<Base> {
        &self.provider
    }

    async fn wait_for_block(&self, min_block: u64) -> Result<u64> {
        timeout(BLOCK_PRODUCTION_TIMEOUT, async {
            loop {
                let block = self.provider.get_block_number().await?;
                if block >= min_block {
                    return Ok::<_, eyre::Error>(block);
                }
                sleep(BLOCK_POLL_INTERVAL).await;
            }
        })
        .await
        .wrap_err("Block production timed out")?
    }

    async fn wait_for_balance(&self, address: alloy_primitives::Address) -> Result<()> {
        timeout(Duration::from_secs(15), async {
            loop {
                let balance = self.provider.get_balance(address).await?;
                if balance > U256::ZERO {
                    return Ok::<_, eyre::Error>(());
                }
                sleep(BLOCK_POLL_INTERVAL).await;
            }
        })
        .await
        .wrap_err("Timed out waiting for funded devnet account")?
    }
}
