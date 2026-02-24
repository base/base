//! Transport configuration types.

use std::env;

/// Default vsock port matching Go's `main.go`.
pub const DEFAULT_VSOCK_PORT: u32 = 1234;

/// Default HTTP port matching Go's `main.go`.
pub const DEFAULT_HTTP_PORT: u16 = 1234;

/// Default HTTP proxy port matching Go's `cmd/server/main.go`.
pub const DEFAULT_PROXY_PORT: u16 = 7333;

/// Default vsock CID for the enclave (from parent's perspective).
pub const DEFAULT_VSOCK_CID: u32 = 16;

/// Default HTTP body limit (50MB) matching Go's `defaultHTTPBodyLimit`.
///
/// This is increased from go-ethereum's default of 5MB because witness data
/// for some blocks can exceed 5MB, causing 413 errors.
pub const DEFAULT_HTTP_BODY_LIMIT: u32 = 50 * 1024 * 1024;

/// Environment variable for configuring HTTP body limit.
pub const HTTP_BODY_LIMIT_ENV: &str = "OP_ENCLAVE_HTTP_BODY_LIMIT";

/// Transport configuration.
#[derive(Debug, Clone)]
pub struct TransportConfig {
    /// Vsock port to listen on.
    pub vsock_port: u32,

    /// HTTP port to listen on (fallback when vsock unavailable).
    pub http_port: u16,

    /// Maximum HTTP body size in bytes.
    pub http_body_limit: u32,
}

impl Default for TransportConfig {
    fn default() -> Self {
        Self {
            vsock_port: DEFAULT_VSOCK_PORT,
            http_port: DEFAULT_HTTP_PORT,
            http_body_limit: Self::http_body_limit_from_env(),
        }
    }
}

impl TransportConfig {
    /// Create a new transport configuration with custom values.
    #[must_use]
    pub const fn new(vsock_port: u32, http_port: u16, http_body_limit: u32) -> Self {
        Self { vsock_port, http_port, http_body_limit }
    }

    /// Get the HTTP body limit from environment or use default.
    #[must_use]
    pub fn http_body_limit_from_env() -> u32 {
        env::var(HTTP_BODY_LIMIT_ENV)
            .ok()
            .and_then(|s| s.parse().ok())
            .filter(|&v| v > 0)
            .unwrap_or(DEFAULT_HTTP_BODY_LIMIT)
    }
}
