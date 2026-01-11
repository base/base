//! Contains the [`MeteringExtension`] which wires up the metering RPC surface
//! on the Base node builder.

use base_client_node::{BaseNodeExtension, FromExtensionConfig, OpBuilder};
use tracing::info;

use crate::{MeteringApiImpl, MeteringApiServer};

/// Helper struct that wires the metering RPC into the node builder.
#[derive(Debug, Clone, Copy)]
pub struct MeteringExtension {
    /// Whether metering is enabled.
    pub enabled: bool,
}

impl MeteringExtension {
    /// Creates a new metering extension.
    pub const fn new(enabled: bool) -> Self {
        Self { enabled }
    }
}

impl BaseNodeExtension for MeteringExtension {
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

impl FromExtensionConfig for MeteringExtension {
    type Config = bool;

    fn from_config(enabled: Self::Config) -> Self {
        Self::new(enabled)
    }
}
