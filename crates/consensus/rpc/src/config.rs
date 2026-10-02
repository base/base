//! Contains the RPC Configuration.

use std::{net::SocketAddr, num::NonZeroUsize, path::PathBuf, time::Duration};

use url::Url;

/// The RPC configuration.
#[derive(Debug, Clone)]
pub struct RpcBuilder {
    /// Prevent the rpc server from being restarted.
    pub no_restart: bool,
    /// The RPC socket address.
    pub socket: SocketAddr,
    /// Enable the admin API.
    pub enable_admin: bool,
    /// File path used to persist state changes made via the admin API so they persist across
    /// restarts.
    pub admin_persistence: Option<PathBuf>,
    /// Enable the websocket rpc server
    pub ws_enabled: bool,
    /// Enable development RPC endpoints
    pub dev_enabled: bool,
    /// HTTP request timeout for the RPC server.
    pub http_timeout: Duration,
    /// Maximum number of concurrent in-flight RPC requests.
    pub max_concurrent_requests: NonZeroUsize,
    /// Upstream JSON-RPC endpoint that receives every method this server does not serve itself.
    ///
    /// Set by the unified binary to the embedded execution node's HTTP server so a single
    /// endpoint answers both consensus and execution namespaces.
    pub forward_unmatched_to: Option<Url>,
}

impl RpcBuilder {
    /// Number of restart attempts made when restarts are allowed.
    const RESTART_ATTEMPTS: u32 = 3;

    /// Returns whether WebSocket RPC endpoint is enabled
    pub const fn ws_enabled(&self) -> bool {
        self.ws_enabled
    }

    /// Returns whether development RPC endpoints are enabled
    pub const fn dev_enabled(&self) -> bool {
        self.dev_enabled
    }

    /// Returns whether the admin RPC namespace is enabled.
    pub const fn admin_enabled(&self) -> bool {
        self.enable_admin
    }

    /// Returns the socket address of the [`RpcBuilder`].
    pub const fn socket(&self) -> SocketAddr {
        self.socket
    }

    /// Returns the number of times the RPC server will attempt to restart if it stops.
    pub const fn restart_count(&self) -> u32 {
        if self.no_restart { 0 } else { Self::RESTART_ATTEMPTS }
    }

    /// Sets the given [`SocketAddr`] on the [`RpcBuilder`].
    pub fn set_addr(self, addr: SocketAddr) -> Self {
        Self { socket: addr, ..self }
    }
}
