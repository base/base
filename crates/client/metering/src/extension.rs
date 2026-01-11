//! Contains the [`MeteringRpcExtension`] which wires up the metering RPC surface
//! on the Base node builder.

use base_primitives::{
    BaseNodeExtension, ConfigurableBaseNodeExtension, FlashblocksConfig, OpBuilder, OpProvider,
};
use base_reth_flashblocks::{FlashblocksCell, FlashblocksState};
use tracing::info;

use crate::{MeteringApiImpl, MeteringApiServer};

/// Helper struct that wires the metering RPC into the node builder.
#[derive(Debug, Clone)]
pub struct MeteringRpcExtension {
    /// Whether metering is enabled.
    pub enabled: bool,
    /// Shared Flashblocks state cache (same as used by FlashblocksRpcExtension).
    pub cell: FlashblocksCell<FlashblocksState<OpProvider>>,
    /// Optional Flashblocks configuration.
    pub flashblocks_config: Option<FlashblocksConfig>,
}

impl MeteringRpcExtension {
    /// Creates a new metering RPC extension.
    pub const fn new(
        enabled: bool,
        cell: FlashblocksCell<FlashblocksState<OpProvider>>,
        flashblocks_config: Option<FlashblocksConfig>,
    ) -> Self {
        Self { enabled, cell, flashblocks_config }
    }
}

impl BaseNodeExtension for MeteringRpcExtension {
    /// Applies the extension to the supplied builder.
    fn apply(self: Box<Self>, builder: OpBuilder) -> OpBuilder {
        if !self.enabled {
            return builder;
        }

        let flashblocks_cell = self.cell;
        let flashblocks_config = self.flashblocks_config;

        builder.extend_rpc_modules(move |ctx| {
            info!(message = "Starting Metering RPC");

            // Get or initialize the flashblocks state using the shared cell.
            // This ensures metering uses the same FlashblocksState as FlashblocksRpcExtension.
            let flashblocks_state = flashblocks_config.as_ref().map(|cfg| {
                flashblocks_cell
                    .get_or_init(|| {
                        std::sync::Arc::new(FlashblocksState::new(
                            ctx.provider().clone(),
                            cfg.max_pending_blocks_depth,
                        ))
                    })
                    .clone()
            });

            // If flashblocks is enabled, use the state; otherwise use a dummy implementation
            if let Some(fb_state) = flashblocks_state {
                let metering_api = MeteringApiImpl::new(ctx.provider().clone(), fb_state);
                ctx.modules.merge_configured(metering_api.into_rpc())?;
            } else {
                // Flashblocks not configured, use a no-op flashblocks state
                let fb_state = std::sync::Arc::new(FlashblocksState::new(
                    ctx.provider().clone(),
                    1, // minimal depth
                ));
                let metering_api = MeteringApiImpl::new(ctx.provider().clone(), fb_state);
                ctx.modules.merge_configured(metering_api.into_rpc())?;
            }

            Ok(())
        })
    }
}

/// Configuration trait for [`MeteringRpcExtension`].
///
/// Types implementing this trait can be used to construct a [`MeteringRpcExtension`].
pub trait MeteringRpcConfig: base_reth_flashblocks::FlashblocksCanonConfig {
    /// Returns whether metering is enabled.
    fn metering_enabled(&self) -> bool;
}

impl<C: MeteringRpcConfig> ConfigurableBaseNodeExtension<C> for MeteringRpcExtension {
    fn build(config: &C) -> eyre::Result<Self> {
        Ok(Self::new(
            config.metering_enabled(),
            config.flashblocks_cell().clone(),
            config.flashblocks().cloned(),
        ))
    }
}
