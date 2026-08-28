//! Shared helpers for system tests.

use std::time::Duration;

use alloy_provider::{Provider, RootProvider};
use base_common_network::Base;
use eyre::{Result, WrapErr};
use tokio::time::{sleep, timeout};

pub(crate) const L1_CHAIN_ID: u64 = 1337;
pub(crate) const L2_CHAIN_ID: u64 = 84538453;
pub(crate) const BASE_AZUL_ACTIVATION_BLOCK: u64 = 0;
pub(crate) const BASE_BERYL_ACTIVATION_BLOCK: u64 = 3;
pub(crate) const BLOCK_PRODUCTION_TIMEOUT: Duration = Duration::from_secs(30);
pub(crate) const BLOCK_POLL_INTERVAL: Duration = Duration::from_millis(500);

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
