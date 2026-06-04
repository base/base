//! Shared helpers for system tests.

use std::time::Duration;

use alloy_primitives::{Address, U256};
use alloy_provider::{Provider, RootProvider};
use base_common_network::Base;
use base_system_tests::{SystemTestStack, SystemTestStackBuilder};
use eyre::{Result, WrapErr};
use tokio::time::{sleep, timeout};

pub(crate) const L1_CHAIN_ID: u64 = 1337;
pub(crate) const L2_CHAIN_ID: u64 = 84538453;
pub(crate) const BASE_AZUL_ACTIVATION_BLOCK: u64 = 0;
pub(crate) const BASE_BERYL_ACTIVATION_BLOCK: u64 = 3;
pub(crate) const BLOCK_PRODUCTION_TIMEOUT: Duration = Duration::from_secs(30);
pub(crate) const BLOCK_POLL_INTERVAL: Duration = Duration::from_millis(500);
pub(crate) const TX_RECEIPT_TIMEOUT: Duration = Duration::from_secs(60);

/// Starts a system test stack with Beryl active at block 3 and waits for block 4.
///
/// The returned [`SystemTestStack`] must be kept alive for the duration of the test;
/// dropping it shuts down the underlying containers.
pub(crate) async fn start_beryl_system() -> Result<(SystemTestStack, RootProvider<Base>)> {
    let system = SystemTestStackBuilder::new()
        .with_l1_chain_id(L1_CHAIN_ID)
        .with_l2_chain_id(L2_CHAIN_ID)
        .with_base_azul_activation_block(BASE_AZUL_ACTIVATION_BLOCK)
        .with_base_beryl_activation_block(BASE_BERYL_ACTIVATION_BLOCK)
        .build()
        .await?;
    let provider = system.l2_builder_provider()?;
    wait_for_block(&provider, BASE_BERYL_ACTIVATION_BLOCK + 1).await?;
    Ok((system, provider))
}

/// Polls until the L2 block number reaches `min_block`.
pub(crate) async fn wait_for_block(provider: &RootProvider<Base>, min_block: u64) -> Result<u64> {
    timeout(BLOCK_PRODUCTION_TIMEOUT, async {
        loop {
            let block = provider.get_block_number().await?;
            if block >= min_block {
                return Ok::<_, eyre::Error>(block);
            }
            sleep(BLOCK_POLL_INTERVAL).await;
        }
    })
    .await
    .wrap_err("Block production timed out")?
}

/// Polls until `address` has a non-zero ETH balance on the L2.
pub(crate) async fn wait_for_balance(
    provider: &RootProvider<Base>,
    address: Address,
) -> Result<()> {
    timeout(Duration::from_secs(15), async {
        loop {
            let balance = provider.get_balance(address).await?;
            if balance > U256::ZERO {
                return Ok::<_, eyre::Error>(());
            }
            sleep(BLOCK_POLL_INTERVAL).await;
        }
    })
    .await
    .wrap_err("Timed out waiting for funded system test account")?
}
