//! Per-client-IP request rate limiting middleware for the telemetry HTTP API.

use std::{
    fmt,
    net::{IpAddr, Ipv6Addr, SocketAddr},
    num::NonZeroU32,
    sync::Arc,
    time::Duration,
};

use axum::{
    extract::{ConnectInfo, Request, State},
    http::{
        StatusCode,
        header::{CONTENT_TYPE, RETRY_AFTER},
    },
    middleware::Next,
    response::{IntoResponse, Response},
};
use base_trusted_proxy::TrustedProxyConfig;
use governor::{
    DefaultKeyedRateLimiter, Quota, RateLimiter,
    clock::{Clock, DefaultClock},
};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::debug;

/// Default P2P probe requests allowed per client IP per minute.
pub const DEFAULT_P2P_PROBE_REQUESTS_PER_MINUTE: NonZeroU32 = NonZeroU32::new(2).unwrap();
/// Interval between stale rate-limit bucket eviction passes.
pub const RATE_LIMIT_EVICTION_INTERVAL: Duration = Duration::from_secs(60);
/// HTTP header carrying the client IP, set by the fronting proxy.
pub const CLIENT_IP_HEADER: &str = "X-Forwarded-For";
/// JSON body returned when a request exceeds a rate limit.
pub const RATE_LIMITED_BODY: &str = r#"{"error":"rate_limited"}"#;

/// Rejection produced when a request exceeds a rate limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateLimitExceeded {
    /// Time until the next request conforms to the quota.
    pub retry_after: Duration,
}

impl RateLimitExceeded {
    /// Returns the `Retry-After` value in whole seconds, rounded up.
    pub fn retry_after_secs(&self) -> u64 {
        let secs = self.retry_after.as_secs() + u64::from(self.retry_after.subsec_nanos() > 0);
        secs.max(1)
    }
}

impl IntoResponse for RateLimitExceeded {
    fn into_response(self) -> Response {
        (
            StatusCode::TOO_MANY_REQUESTS,
            [
                (RETRY_AFTER, self.retry_after_secs().to_string()),
                (CONTENT_TYPE, "application/json".to_string()),
            ],
            RATE_LIMITED_BODY,
        )
            .into_response()
    }
}

/// GCRA rate limiter keyed by client IP, applied as Axum middleware.
pub struct IpRateLimiter {
    limiter: DefaultKeyedRateLimiter<IpAddr>,
    clock: DefaultClock,
}

impl fmt::Debug for IpRateLimiter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IpRateLimiter").finish_non_exhaustive()
    }
}

impl IpRateLimiter {
    /// Creates a limiter allowing `per_minute` requests per key.
    pub fn per_minute(per_minute: NonZeroU32) -> Self {
        let clock = DefaultClock::default();
        let quota = Quota::per_minute(per_minute);
        Self { limiter: RateLimiter::dashmap_with_clock(quota, clock.clone()), clock }
    }

    /// Creates a limiter allowing `per_hour` requests per key.
    pub fn per_hour(per_hour: NonZeroU32) -> Self {
        let clock = DefaultClock::default();
        let quota = Quota::per_hour(per_hour);
        Self { limiter: RateLimiter::dashmap_with_clock(quota, clock.clone()), clock }
    }

    /// Returns the rate-limit bucket key for `ip`.
    ///
    /// `IPv4` addresses key individually, while `IPv6` addresses key by
    /// their /64 prefix: clients routinely hold an entire /64 delegation, so
    /// keying on exact addresses would let them defeat the quota by rotating
    /// the interface identifier on every request.
    pub const fn bucket_key(ip: IpAddr) -> IpAddr {
        match ip.to_canonical() {
            ip @ IpAddr::V4(_) => ip,
            IpAddr::V6(v6) => {
                let seg = v6.segments();
                IpAddr::V6(Ipv6Addr::new(seg[0], seg[1], seg[2], seg[3], 0, 0, 0, 0))
            }
        }
    }

    /// Checks one request for `ip`, returning the retry delay when limited.
    pub fn check(&self, ip: IpAddr) -> Result<(), RateLimitExceeded> {
        self.limiter.check_key(&Self::bucket_key(ip)).map_err(|not_until| RateLimitExceeded {
            retry_after: not_until.wait_time_from(self.clock.now()),
        })
    }

    /// Spawns a background task that periodically evicts stale buckets until
    /// the token is cancelled.
    pub fn spawn_eviction_task(self: &Arc<Self>, cancel: CancellationToken) -> JoinHandle<()> {
        let this = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    () = cancel.cancelled() => break,
                    () = tokio::time::sleep(RATE_LIMIT_EVICTION_INTERVAL) => this.limiter.retain_recent(),
                }
            }
        })
    }
}

/// Axum middleware state enforcing the per-client-IP request quota.
#[derive(Debug, Clone)]
pub struct PerIpRateLimit {
    limiter: Arc<IpRateLimiter>,
    proxy: Arc<TrustedProxyConfig>,
}

impl PerIpRateLimit {
    /// Creates middleware state from a limiter and trusted proxy configuration.
    pub const fn new(limiter: Arc<IpRateLimiter>, proxy: Arc<TrustedProxyConfig>) -> Self {
        Self { limiter, proxy }
    }

    /// Returns the trusted proxy configuration used to resolve the client IP.
    pub const fn proxy(&self) -> &Arc<TrustedProxyConfig> {
        &self.proxy
    }

    /// Axum middleware entry point; use with `middleware::from_fn_with_state`.
    pub async fn enforce(
        State(state): State<Self>,
        ConnectInfo(peer): ConnectInfo<SocketAddr>,
        request: Request,
        next: Next,
    ) -> Response {
        let client_ip = state.proxy.client_ip(peer.ip(), request.headers());
        match state.limiter.check(client_ip) {
            Ok(()) => next.run(request).await,
            Err(exceeded) => {
                debug!(client = %client_ip, retry_after_secs = exceeded.retry_after_secs(), "request rate limited");
                exceeded.into_response()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};

    use axum::http::header::RETRY_AFTER;

    use super::*;

    const fn nz(value: u32) -> NonZeroU32 {
        NonZeroU32::new(value).unwrap()
    }

    #[test]
    fn limits_after_quota_and_keys_independently() {
        let limiter = IpRateLimiter::per_minute(nz(2));
        let first = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1));
        let second = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 2));

        assert!(limiter.check(first).is_ok());
        assert!(limiter.check(first).is_ok());
        let exceeded = limiter.check(first).unwrap_err();
        assert!(exceeded.retry_after > Duration::ZERO);
        assert!(limiter.check(second).is_ok());
    }

    #[test]
    fn ipv6_clients_share_one_bucket_per_64_prefix() {
        let limiter = IpRateLimiter::per_minute(nz(1));
        let first: IpAddr = "2001:db8:1:2:aaaa::1".parse().unwrap();
        let rotated: IpAddr = "2001:db8:1:2:bbbb::2".parse().unwrap();
        let other_prefix: IpAddr = "2001:db8:1:3::1".parse().unwrap();

        assert!(limiter.check(first).is_ok());
        assert!(limiter.check(rotated).is_err());
        assert!(limiter.check(other_prefix).is_ok());
    }

    #[test]
    fn retry_after_rounds_up_to_at_least_one_second() {
        assert_eq!(
            RateLimitExceeded { retry_after: Duration::from_millis(1) }.retry_after_secs(),
            1
        );
        assert_eq!(RateLimitExceeded { retry_after: Duration::ZERO }.retry_after_secs(), 1);
        assert_eq!(
            RateLimitExceeded { retry_after: Duration::from_millis(2500) }.retry_after_secs(),
            3
        );
    }

    #[test]
    fn rate_limited_response_carries_status_and_retry_after() {
        let response = RateLimitExceeded { retry_after: Duration::from_secs(7) }.into_response();

        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(response.headers().get(RETRY_AFTER).unwrap(), "7");
    }
}
