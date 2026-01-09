//! Contains the [`MeteringRpcExtension`] which wires up the metering RPC surface
//! on the Base node builder.

use base_primitives::{BaseNodeExtension, ConfigurableBaseNodeExtension, OpBuilder};
use tracing::info;

use crate::{MeteringApiImpl, MeteringApiServer};

/// Helper struct that wires the metering RPC into the node builder.
#[derive(Debug, Clone, Copy)]
pub struct MeteringRpcExtension {
    /// Whether metering is enabled.
    pub enabled: bool,
}

impl MeteringRpcExtension {
    /// Creates a new metering RPC extension.
    pub const fn new(enabled: bool) -> Self {
        Self { enabled }
    }
}

impl BaseNodeExtension for MeteringRpcExtension {
    /// Applies the extension to the supplied builder.
    fn apply(self: Box<Self>, builder: OpBuilder) -> OpBuilder {
        if !self.enabled {
            return builder;
        }

        builder.extend_rpc_modules(move |ctx| {
            info!(message = "Starting Metering RPC");
            let metering_api = MeteringApiImpl::new(ctx.provider().clone());
            ctx.modules.merge_configured(metering_api.into_rpc())?;
            Ok(())
        })
    }
}

/// Configuration trait for [`MeteringRpcExtension`].
///
/// Types implementing this trait can be used to construct a [`MeteringRpcExtension`].
pub trait MeteringRpcConfig {
    /// Returns whether metering is enabled.
    fn metering_enabled(&self) -> bool;
}

impl<C: MeteringRpcConfig> ConfigurableBaseNodeExtension<C> for MeteringRpcExtension {
    fn build(config: &C) -> eyre::Result<Self> {
        Ok(Self::new(config.metering_enabled()))
    }
}
