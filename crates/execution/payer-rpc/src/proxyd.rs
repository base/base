//! Errors proxyd answers with itself instead of relaying the payer's response.

/// JSON-RPC errors proxyd returns in place of the payer's response, when it did not forward the
/// request or got no usable answer: the payer backend is offline, unhealthy, over capacity,
/// slow, or malformed, or the caller is over a rate limit.
///
/// The codes are proxyd's own, from `backend.go` in `base/infra-routing`. ERC-8168 payer errors
/// use `-32000`, so these cannot be mistaken for the payer's answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProxydError;

impl ProxydError {
    /// The payer backend is offline.
    pub const BACKEND_OFFLINE: i64 = -32010;
    /// No payer backend is healthy.
    pub const NO_HEALTHY_BACKEND: i64 = -32011;
    /// The payer backend is over capacity.
    pub const BACKEND_OVER_CAPACITY: i64 = -32012;
    /// The payer backend returned an invalid response.
    pub const BACKEND_BAD_RESPONSE: i64 = -32013;
    /// The payer backend did not answer in time.
    pub const GATEWAY_TIMEOUT: i64 = -32015;
    /// The caller is over a rate limit.
    pub const OVER_RATE_LIMIT: i64 = -32016;
    /// The payer backend is not healthy.
    pub const BACKEND_NOT_HEALTHY: i64 = -32018;
    /// Too many requests are in flight.
    pub const TOO_MANY_REQUESTS: i64 = -32024;

    /// Returns whether `code` is one of these errors, so the payer itself did not answer.
    pub const fn is_proxyd_error(code: i64) -> bool {
        matches!(
            code,
            Self::BACKEND_OFFLINE
                | Self::NO_HEALTHY_BACKEND
                | Self::BACKEND_OVER_CAPACITY
                | Self::BACKEND_BAD_RESPONSE
                | Self::GATEWAY_TIMEOUT
                | Self::OVER_RATE_LIMIT
                | Self::BACKEND_NOT_HEALTHY
                | Self::TOO_MANY_REQUESTS
        )
    }
}
