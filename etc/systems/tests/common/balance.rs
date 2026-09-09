//! Shared L2 funding and receipt waits.

use std::time::Duration;

use alloy_primitives::{Address, U256};
use alloy_provider::{Provider, RootProvider};
use base_common_network::Base;
use eyre::{Result, WrapErr};
use tokio::time::{sleep, timeout};

const BLOCK_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Receipt wait used by B-20, registry, and EIP-8130 tests.
pub(crate) const TX_RECEIPT_TIMEOUT: Duration = Duration::from_secs(60);

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
