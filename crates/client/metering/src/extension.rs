//! Contains the [`MeteringExtension`] which wires up the metering RPC surface
//! on the Base node builder.

use std::sync::Arc;

use base_client_node::{BaseNodeExtension, FromExtensionConfig, OpBuilder};
use base_flashblocks::{FlashblocksConfig, FlashblocksState};
use tracing::info;

use crate::{MeteringApiImpl, MeteringApiServer};

/// Helper struct that wires the metering RPC into the node builder.
#[derive(Debug)]
pub struct MeteringExtension {
    /// Whether metering is enabled.
    pub enabled: bool,
    /// Optional Flashblocks configuration (includes state).
    pub flashblocks_config: Option<FlashblocksConfig>,
}

impl MeteringExtension {
    /// Creates a new metering extension.
    pub const fn new(enabled: bool, flashblocks_config: Option<FlashblocksConfig>) -> Self {
        Self { enabled, flashblocks_config }
    }
}

impl BaseNodeExtension for MeteringExtension {
    /// Applies the extension to the supplied builder.
    fn apply(self: Box<Self>, builder: OpBuilder) -> OpBuilder {
        if !self.enabled {
            return builder;
        }

        let flashblocks_config = self.flashblocks_config;

        builder.extend_rpc_modules(move |ctx| {
            info!(message = "Starting Metering RPC");

            // Get flashblocks state from config, or create a default one if not configured
            let fb_state: Arc<FlashblocksState> =
                flashblocks_config.as_ref().map(|cfg| cfg.state.clone()).unwrap_or_default();

            let metering_api = MeteringApiImpl::new(ctx.provider().clone(), fb_state);
            ctx.modules.merge_configured(metering_api.into_rpc())?;

            Ok(())
        })
    }
}

/// Configuration for building a [`MeteringExtension`].
#[derive(Debug)]
pub struct MeteringConfig {
    /// Whether metering is enabled.
    pub enabled: bool,
    /// Optional Flashblocks configuration (includes state).
    pub flashblocks_config: Option<FlashblocksConfig>,
}

impl MeteringConfig {
    /// Creates a configuration with metering enabled and no flashblocks integration.
    pub const fn enabled() -> Self {
        Self { enabled: true, flashblocks_config: None }
    }

    /// Creates a configuration with metering enabled and flashblocks integration.
    pub const fn with_flashblocks(flashblocks_config: FlashblocksConfig) -> Self {
        Self { enabled: true, flashblocks_config: Some(flashblocks_config) }
    }
}

impl FromExtensionConfig for MeteringExtension {
    type Config = MeteringConfig;

    fn from_config(config: Self::Config) -> Self {
        Self::new(config.enabled, config.flashblocks_config)
    }
}
