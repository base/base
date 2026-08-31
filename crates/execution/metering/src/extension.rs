//! Contains the [`MeteringExtension`] which wires up the metering RPC surface
//! on the Base node builder.

use std::sync::Arc;

use base_flashblocks::FlashblocksConfig;
use base_node_runner::{BaseNodeExtension, FromExtensionConfig, NodeHooks};
use tracing::info;

use crate::{MeteredOpcodes, MeteringApiImpl, MeteringApiServer};

/// Helper struct that wires the metering RPC into the node builder.
#[derive(Debug, Default)]
pub struct MeteringExtension {
    /// Whether metering is enabled.
    pub enabled: bool,
    /// Optional Flashblocks configuration (includes state).
    pub flashblocks_config: Option<FlashblocksConfig>,
    /// Opcodes and precompiles to track for gas metering.
    pub metered_opcodes: MeteredOpcodes,
}

impl MeteringExtension {
    /// Creates a new metering extension.
    pub fn new(enabled: bool, flashblocks_config: Option<FlashblocksConfig>) -> Self {
        Self { enabled, flashblocks_config, metered_opcodes: MeteredOpcodes::default() }
    }

    /// Sets the opcodes and precompiles to track for gas metering.
    pub fn with_metered_opcodes(mut self, opcodes: MeteredOpcodes) -> Self {
        self.metered_opcodes = opcodes;
        self
    }
}

impl BaseNodeExtension for MeteringExtension {
    /// Applies the extension to the supplied hooks.
    fn apply(self: Box<Self>, hooks: NodeHooks) -> NodeHooks {
        if !self.enabled {
            return hooks;
        }

        let metered_opcodes = Arc::new(self.metered_opcodes);

        hooks.add_rpc_module(move |ctx| {
            info!("starting metering RPC");
            let metering_api =
                MeteringApiImpl::new(ctx.provider().clone(), Arc::clone(&metered_opcodes));

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
    /// Opcodes and precompiles to track for gas metering.
    pub metered_opcodes: MeteredOpcodes,
}

impl MeteringConfig {
    /// Creates a configuration with metering disabled.
    pub fn disabled() -> Self {
        Self { enabled: false, ..Self::enabled() }
    }

    /// Creates a configuration with metering enabled and no flashblocks integration.
    pub fn enabled() -> Self {
        Self {
            enabled: true,
            flashblocks_config: None,
            metered_opcodes: MeteredOpcodes::default(),
        }
    }

    /// Creates a configuration with metering enabled and flashblocks integration.
    pub fn with_flashblocks(flashblocks_config: FlashblocksConfig) -> Self {
        Self {
            enabled: true,
            flashblocks_config: Some(flashblocks_config),
            metered_opcodes: MeteredOpcodes::default(),
        }
    }

    /// Sets the opcodes and precompiles to track for gas metering.
    pub fn with_metered_opcodes(mut self, opcodes: MeteredOpcodes) -> Self {
        self.metered_opcodes = opcodes;
        self
    }
}

impl FromExtensionConfig for MeteringExtension {
    type Config = MeteringConfig;

    fn from_config(config: Self::Config) -> Self {
        Self {
            enabled: config.enabled,
            flashblocks_config: config.flashblocks_config,
            metered_opcodes: config.metered_opcodes,
        }
    }
}
