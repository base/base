//! Contains the [`DashboardExtension`] which wires up the monitoring dashboard
//! on the Base node builder.

use std::{net::SocketAddr, sync::Arc};

use base_client_node::{BaseBuilder, BaseNodeExtension, FromExtensionConfig};
use tracing::info;

use crate::server::start_dashboard_server;

/// Extension that adds the monitoring dashboard to the Base node.
#[derive(Debug)]
pub struct DashboardExtension {
    /// Whether the dashboard is enabled.
    enabled: bool,
    /// Socket address to bind the HTTP server.
    bind_addr: SocketAddr,
}

impl DashboardExtension {
    /// Creates a new dashboard extension.
    pub const fn new(enabled: bool, bind_addr: SocketAddr) -> Self {
        Self { enabled, bind_addr }
    }
}

impl BaseNodeExtension for DashboardExtension {
    fn apply(self: Box<Self>, builder: BaseBuilder) -> BaseBuilder {
        if !self.enabled {
            info!(message = "Dashboard is disabled");
            return builder;
        }

        let bind_addr = self.bind_addr;

        builder.add_node_started_hook(move |ctx| {
            info!(addr = %bind_addr, "Starting monitoring dashboard");

            let provider = ctx.provider().clone();
            let pool = ctx.pool().clone();

            // Peer counts are not easily accessible without more complex network APIs
            // For now, provide a simple placeholder that returns zeros
            let peers_fn: Arc<dyn Fn() -> (usize, usize, usize) + Send + Sync> =
                Arc::new(|| (0, 0, 0));

            // Get network and client info from the chain config
            let network_name = ctx
                .config
                .chain
                .chain()
                .named()
                .map(|c| c.to_string())
                .unwrap_or_else(|| format!("chain-{}", ctx.config.chain.chain().id()));
            let client_name = format!("base-reth/{}", env!("CARGO_PKG_VERSION"));

            tokio::spawn(start_dashboard_server(
                bind_addr,
                provider,
                pool,
                peers_fn,
                network_name,
                client_name,
            ));

            Ok(())
        })
    }
}

/// Configuration for building a [`DashboardExtension`].
#[derive(Debug, Clone)]
pub struct DashboardConfig {
    /// Whether the dashboard is enabled.
    pub enabled: bool,
    /// Socket address to bind the HTTP server.
    pub bind_addr: SocketAddr,
}

impl DashboardConfig {
    /// Creates a new configuration with the dashboard enabled on the specified address.
    pub const fn new(bind_addr: SocketAddr) -> Self {
        Self { enabled: true, bind_addr }
    }

    /// Creates a disabled configuration.
    pub const fn disabled() -> Self {
        Self {
            enabled: false,
            bind_addr: SocketAddr::new(
                std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1)),
                8080,
            ),
        }
    }
}

impl Default for DashboardConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bind_addr: SocketAddr::new(
                std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1)),
                8080,
            ),
        }
    }
}

impl FromExtensionConfig for DashboardExtension {
    type Config = DashboardConfig;

    fn from_config(config: Self::Config) -> Self {
        Self::new(config.enabled, config.bind_addr)
    }
}
