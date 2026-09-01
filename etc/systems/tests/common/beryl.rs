//! Beryl stack helpers. Path-included only by tests that activate Beryl.

use alloy_provider::RootProvider;
use base_common_network::Base;
use base_system_tests::{SystemTestStack, SystemTestStackBuilder};
use eyre::Result;

pub(crate) use super::balance::{TX_RECEIPT_TIMEOUT, wait_for_balance};
use super::common::{
    BASE_AZUL_ACTIVATION_BLOCK, BASE_BERYL_ACTIVATION_BLOCK, L1_CHAIN_ID, L2_CHAIN_ID,
    wait_for_block,
};

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
