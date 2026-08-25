//! Cobalt stack helper. Not a `common` submodule so Beryl-only test binaries do not compile it.

use std::time::Duration;

use alloy_provider::{Provider, RootProvider};
use base_common_network::Base;
use base_system_tests::{SystemTestStack, SystemTestStackBuilder};
use eyre::{Result, WrapErr};
use tokio::time::{sleep, timeout};

const L1_CHAIN_ID: u64 = 1337;
const L2_CHAIN_ID: u64 = 84538453;
const BASE_AZUL_ACTIVATION_BLOCK: u64 = 0;
const BASE_BERYL_ACTIVATION_BLOCK: u64 = 3;
const BASE_COBALT_ACTIVATION_BLOCK: u64 = 5;
const BLOCK_PRODUCTION_TIMEOUT: Duration = Duration::from_secs(30);
const BLOCK_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Starts a system test stack with Cobalt active at block 5 and waits for block 6.
///
/// The returned [`SystemTestStack`] must be kept alive for the duration of the test;
/// dropping it shuts down the underlying containers.
pub(crate) async fn start_cobalt_system() -> Result<(SystemTestStack, RootProvider<Base>)> {
    let system = SystemTestStackBuilder::new()
        .with_l1_chain_id(L1_CHAIN_ID)
        .with_l2_chain_id(L2_CHAIN_ID)
        .with_base_azul_activation_block(BASE_AZUL_ACTIVATION_BLOCK)
        .with_base_beryl_activation_block(BASE_BERYL_ACTIVATION_BLOCK)
        .with_base_cobalt_activation_block(BASE_COBALT_ACTIVATION_BLOCK)
        .build()
        .await?;
    let provider = system.l2_builder_provider()?;
    timeout(BLOCK_PRODUCTION_TIMEOUT, async {
        loop {
            let block = provider.get_block_number().await?;
            if block > BASE_COBALT_ACTIVATION_BLOCK {
                return Ok::<_, eyre::Error>(block);
            }
            sleep(BLOCK_POLL_INTERVAL).await;
        }
    })
    .await
    .wrap_err("Block production timed out")??;
    Ok((system, provider))
}
