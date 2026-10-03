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
    /// Lets a single endpoint answer both consensus and execution namespaces. The unified binary
    /// points it at the embedded execution node's HTTP server.
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

    /// Returns whether `upstream` points at this server's own listening socket.
    ///
    /// Only literal IPs and `localhost` are compared, without resolving DNS. A loopback upstream
    /// matches when the server listens on loopback or on all interfaces, and any other address
    /// matches only when it is exactly the listening address.
    pub fn forwards_to_self(&self, upstream: &Url) -> bool {
        if upstream.port_or_known_default() != Some(self.socket.port()) {
            return false;
        }
        let listens_on_loopback =
            self.socket.ip().is_loopback() || self.socket.ip().is_unspecified();
        match upstream.host() {
            Some(url::Host::Domain(domain)) => {
                domain.eq_ignore_ascii_case("localhost") && listens_on_loopback
            }
            Some(url::Host::Ipv4(ip)) => {
                let ip = std::net::IpAddr::V4(ip);
                (ip.is_loopback() && listens_on_loopback) || ip == self.socket.ip()
            }
            Some(url::Host::Ipv6(ip)) => {
                let ip = std::net::IpAddr::V6(ip);
                (ip.is_loopback() && listens_on_loopback) || ip == self.socket.ip()
            }
            None => false,
        }
    }

    /// Sets the given [`SocketAddr`] on the [`RpcBuilder`].
    pub fn set_addr(self, addr: SocketAddr) -> Self {
        Self { socket: addr, ..self }
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    fn builder(socket: &str) -> RpcBuilder {
        RpcBuilder {
            no_restart: false,
            socket: socket.parse().unwrap(),
            enable_admin: false,
            admin_persistence: None,
            ws_enabled: false,
            dev_enabled: false,
            http_timeout: Duration::from_secs(1),
            max_concurrent_requests: NonZeroUsize::new(1).unwrap(),
            forward_unmatched_to: None,
        }
    }

    #[rstest]
    #[case::loopback_on_wildcard("0.0.0.0:9545", "http://127.0.0.1:9545", true)]
    #[case::localhost_on_wildcard("0.0.0.0:9545", "http://localhost:9545", true)]
    #[case::loopback_on_loopback("127.0.0.1:9545", "http://127.0.0.1:9545", true)]
    #[case::ipv6_loopback_on_wildcard("[::]:9545", "http://[::1]:9545", true)]
    #[case::exact_address("10.0.0.5:9545", "http://10.0.0.5:9545", true)]
    #[case::different_port("0.0.0.0:9545", "http://127.0.0.1:8545", false)]
    #[case::remote_host_on_wildcard("0.0.0.0:9545", "http://10.0.0.9:9545", false)]
    #[case::loopback_when_bound_elsewhere("10.0.0.5:9545", "http://127.0.0.1:9545", false)]
    #[case::domain_on_wildcard("0.0.0.0:9545", "http://el.internal:9545", false)]
    fn detects_forwarding_to_self(
        #[case] socket: &str,
        #[case] upstream: &str,
        #[case] expected: bool,
    ) {
        assert_eq!(builder(socket).forwards_to_self(&Url::parse(upstream).unwrap()), expected);
    }
}
