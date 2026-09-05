//! Cobalt stack helper. Path-included only by tests that activate Cobalt.

use alloy_provider::RootProvider;
use base_common_network::Base;
use base_system_tests::{SystemTestStack, SystemTestStackBuilder};
use eyre::Result;

use super::common::{
    BASE_AZUL_ACTIVATION_BLOCK, BASE_BERYL_ACTIVATION_BLOCK, L1_CHAIN_ID, L2_CHAIN_ID,
    wait_for_block,
};

pub(crate) const BASE_COBALT_ACTIVATION_BLOCK: u64 = 5;

/// Starts a system test stack with Cobalt active at block 5 and waits for block 6.
///
/// The returned [`SystemTestStack`] must be kept alive for the duration of the test;
/// dropping it shuts down the underlying containers.
pub(crate) async fn start_cobalt_system() -> Result<(SystemTestStack, RootProvider<Base>)> {
    start_cobalt_stack(SystemTestStackBuilder::new()).await
}

/// Same as [`start_cobalt_system`], with extra [`SystemTestStackBuilder`] options applied first.
pub(crate) async fn start_cobalt_stack(
    builder: SystemTestStackBuilder,
) -> Result<(SystemTestStack, RootProvider<Base>)> {
    let system = builder
        .with_l1_chain_id(L1_CHAIN_ID)
        .with_l2_chain_id(L2_CHAIN_ID)
        .with_base_azul_activation_block(BASE_AZUL_ACTIVATION_BLOCK)
        .with_base_beryl_activation_block(BASE_BERYL_ACTIVATION_BLOCK)
        .with_base_cobalt_activation_block(BASE_COBALT_ACTIVATION_BLOCK)
        .with_payload_builder_cutover()
        .build()
        .await?;
    let provider = system.l2_builder_provider()?;
    wait_for_block(&provider, BASE_COBALT_ACTIVATION_BLOCK + 1).await?;
    Ok((system, provider))
}
