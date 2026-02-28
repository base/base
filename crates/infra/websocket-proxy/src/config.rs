use std::net::SocketAddr;

use axum::http::Uri;
use base_cli_utils::LogConfig;

use crate::{Authentication, SubscriberOptions};

/// Configuration for the metrics server used by the websocket proxy.
#[derive(Debug, Clone)]
pub struct MetricsServerConfig {
    /// Port to serve metrics on.
    pub port: u16,
    /// Global labels to add to all Prometheus metrics.
    pub global_labels: Vec<(String, String)>,
}

/// Configuration for client ping/pong health checks.
#[derive(Debug, Clone)]
pub struct ClientPingConfig {
    /// Whether client ping/pong health checks are enabled.
    pub enabled: bool,
    /// Interval in seconds between ping messages sent to clients.
    pub interval_secs: u64,
    /// Timeout in milliseconds to wait for pong responses from clients.
    pub pong_timeout_ms: u64,
}

/// Complete configuration for the websocket proxy service.
#[derive(Debug, Clone)]
pub struct WebsocketProxyConfig {
    /// Address and port to listen on for incoming connections.
    pub listen_addr: SocketAddr,
    /// Upstream WebSocket URIs to subscribe to.
    pub upstream_ws: Vec<Uri>,
    /// Number of messages to buffer for lagging clients.
    pub message_buffer_size: usize,
    /// Maximum concurrent connections per instance.
    pub instance_connection_limit: usize,
    /// Maximum concurrent connections per IP.
    pub per_ip_connection_limit: usize,
    /// Whether brotli compression is enabled for downstream messages.
    pub enable_compression: bool,
    /// HTTP header used to determine client origin IP.
    pub ip_addr_http_header: String,
    /// Options for upstream subscriber connections.
    pub subscriber_options: SubscriberOptions,
    /// Optional authentication configuration.
    pub authentication: Option<Authentication>,
    /// Optional metrics server configuration.
    pub metrics_config: Option<MetricsServerConfig>,
    /// Whether unauthenticated public access is allowed alongside API keys.
    pub public_access_enabled: bool,
    /// Client ping/pong health check configuration.
    pub client_ping: ClientPingConfig,
    /// Timeout in milliseconds for sending messages to clients.
    pub client_send_timeout_ms: u64,
    /// Logging configuration.
    pub log: LogConfig,
}
