use std::{
    collections::HashMap,
    fmt,
    net::IpAddr,
    sync::{Arc, Mutex},
};

use thiserror::Error;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tracing::{debug, warn};

/// Type of rate limit that was exceeded.
#[derive(Debug, Clone)]
pub enum RateLimitType {
    /// Per-IP connection limit.
    PerIp,
    /// Global instance connection limit.
    Global,
}

/// Error returned when a rate limit is exceeded.
#[derive(Error, Debug)]
pub enum RateLimitError {
    /// The rate limit was reached.
    #[error("Rate Limit Reached: {reason}")]
    Limit {
        /// Human-readable reason for the rate limit.
        reason: String,
        /// Which type of limit was exceeded.
        limit_type: RateLimitType,
    },
}

/// A ticket representing an active connection that counts against rate limits.
///
/// When dropped, the connection slot is released.
#[clippy::has_significant_drop]
pub struct Ticket {
    addr: IpAddr,
    _permit: OwnedSemaphorePermit,
    rate_limiter: Arc<dyn RateLimit>,
}

impl fmt::Debug for Ticket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Ticket").field("addr", &self.addr).finish_non_exhaustive()
    }
}

impl Drop for Ticket {
    fn drop(&mut self) {
        self.rate_limiter.release(self.addr)
    }
}

/// Trait for rate limiting connection attempts.
pub trait RateLimit: Send + Sync {
    /// Try to acquire a connection slot for the given address.
    fn try_acquire(self: Arc<Self>, addr: IpAddr) -> Result<Ticket, RateLimitError>;

    /// Release a connection slot for the given address.
    fn release(&self, ticket: IpAddr);
}

#[derive(Debug)]
struct Inner {
    active_connections: HashMap<IpAddr, usize>,
    semaphore: Arc<Semaphore>,
}

/// In-memory rate limiter that tracks connections per IP and globally.
#[derive(Debug)]
pub struct InMemoryRateLimit {
    per_ip_limit: usize,
    inner: Mutex<Inner>,
}

impl InMemoryRateLimit {
    /// Create a new in-memory rate limiter with the given limits.
    pub fn new(instance_limit: usize, per_ip_limit: usize) -> Self {
        Self {
            per_ip_limit,
            inner: Mutex::new(Inner {
                active_connections: HashMap::new(),
                semaphore: Arc::new(Semaphore::new(instance_limit)),
            }),
        }
    }
}

impl RateLimit for InMemoryRateLimit {
    fn try_acquire(self: Arc<Self>, addr: IpAddr) -> Result<Ticket, RateLimitError> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());

        let permit = Arc::clone(&inner.semaphore).try_acquire_owned().map_err(|_| {
            RateLimitError::Limit {
                reason: "Global limit".to_owned(),
                limit_type: RateLimitType::Global,
            }
        })?;

        let current_count = inner.active_connections.get(&addr).copied().unwrap_or(0);

        if current_count + 1 > self.per_ip_limit {
            debug!(message = "Rate limit exceeded, trying to acquire", client = addr.to_string());
            return Err(RateLimitError::Limit {
                reason: String::from("IP limit exceeded"),
                limit_type: RateLimitType::PerIp,
            });
        }

        let new_count = current_count + 1;

        inner.active_connections.insert(addr, new_count);

        let rate_limiter: Arc<dyn RateLimit> = Arc::clone(&self) as Arc<dyn RateLimit>;
        Ok(Ticket { addr, _permit: permit, rate_limiter })
    }

    fn release(&self, addr: IpAddr) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());

        let current_count = inner.active_connections.get(&addr).copied().unwrap_or(0);

        let new_count = if current_count == 0 {
            warn!(
                message = "ip counting is not accurate -- unexpected underflow",
                client = addr.to_string()
            );
            0
        } else {
            current_count - 1
        };

        if new_count == 0 {
            inner.active_connections.remove(&addr);
        } else {
            inner.active_connections.insert(addr, new_count);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;

    const GLOBAL_LIMIT: usize = 3;
    const PER_IP_LIMIT: usize = 2;

    #[tokio::test]
    async fn test_tickets_are_released() {
        let user_1 = IpAddr::from_str("127.0.0.1").unwrap();

        let rate_limiter = Arc::new(InMemoryRateLimit::new(GLOBAL_LIMIT, PER_IP_LIMIT));

        assert_eq!(rate_limiter.inner.lock().unwrap().semaphore.available_permits(), GLOBAL_LIMIT);
        assert_eq!(rate_limiter.inner.lock().unwrap().active_connections.len(), 0);

        let c1 = Arc::clone(&rate_limiter).try_acquire(user_1).unwrap();

        assert_eq!(
            rate_limiter.inner.lock().unwrap().semaphore.available_permits(),
            GLOBAL_LIMIT - 1
        );
        assert_eq!(rate_limiter.inner.lock().unwrap().active_connections.len(), 1);
        assert_eq!(rate_limiter.inner.lock().unwrap().active_connections[&user_1], 1);

        drop(c1);

        assert_eq!(rate_limiter.inner.lock().unwrap().semaphore.available_permits(), GLOBAL_LIMIT);
        assert_eq!(rate_limiter.inner.lock().unwrap().active_connections.len(), 0);
    }

    #[tokio::test]
    async fn test_global_rate_limits() {
        let user_1 = IpAddr::from_str("127.0.0.1").unwrap();
        let user_2 = IpAddr::from_str("128.0.0.1").unwrap();

        let rate_limiter = Arc::new(InMemoryRateLimit::new(GLOBAL_LIMIT, PER_IP_LIMIT));

        let _c1 = Arc::clone(&rate_limiter).try_acquire(user_1).unwrap();

        let _c2 = Arc::clone(&rate_limiter).try_acquire(user_2).unwrap();

        let _c3 = Arc::clone(&rate_limiter).try_acquire(user_1).unwrap();

        assert_eq!(rate_limiter.inner.lock().unwrap().semaphore.available_permits(), 0);

        let c4 = Arc::clone(&rate_limiter).try_acquire(user_2);
        assert!(c4.is_err());
        assert_eq!(c4.err().unwrap().to_string(), "Rate Limit Reached: Global limit");

        drop(_c3);

        let c4 = rate_limiter.try_acquire(user_2);
        assert!(c4.is_ok());
    }

    #[tokio::test]
    async fn test_per_ip_limits() {
        let user_1 = IpAddr::from_str("127.0.0.1").unwrap();
        let user_2 = IpAddr::from_str("127.0.0.2").unwrap();

        let rate_limiter = Arc::new(InMemoryRateLimit::new(GLOBAL_LIMIT, PER_IP_LIMIT));

        let _c1 = Arc::clone(&rate_limiter).try_acquire(user_1).unwrap();
        let _c2 = Arc::clone(&rate_limiter).try_acquire(user_1).unwrap();

        assert_eq!(rate_limiter.inner.lock().unwrap().active_connections[&user_1], 2);

        let c3 = Arc::clone(&rate_limiter).try_acquire(user_1);
        assert!(c3.is_err());
        assert_eq!(c3.err().unwrap().to_string(), "Rate Limit Reached: IP limit exceeded");

        let c4 = rate_limiter.try_acquire(user_2);
        assert!(c4.is_ok());
    }

    #[tokio::test]
    async fn test_global_limits_with_multiple_ips() {
        let user_1 = IpAddr::from_str("127.0.0.1").unwrap();
        let user_2 = IpAddr::from_str("127.0.0.2").unwrap();
        let user_3 = IpAddr::from_str("127.0.0.3").unwrap();

        let rate_limiter = Arc::new(InMemoryRateLimit::new(4, 3));

        let ticket_1_1 = Arc::clone(&rate_limiter).try_acquire(user_1).unwrap();
        let ticket_1_2 = Arc::clone(&rate_limiter).try_acquire(user_1).unwrap();

        let ticket_2_1 = Arc::clone(&rate_limiter).try_acquire(user_2).unwrap();
        let ticket_2_2 = Arc::clone(&rate_limiter).try_acquire(user_2).unwrap();

        assert_eq!(rate_limiter.inner.lock().unwrap().semaphore.available_permits(), 0);

        // Try user_3 - should fail due to global limit
        let result = Arc::clone(&rate_limiter).try_acquire(user_3);
        assert!(result.is_err());
        assert_eq!(result.err().unwrap().to_string(), "Rate Limit Reached: Global limit");

        drop(ticket_1_1);

        let ticket_3_1 = Arc::clone(&rate_limiter).try_acquire(user_3).unwrap();

        drop(ticket_1_2);
        drop(ticket_2_1);
        drop(ticket_2_2);
        drop(ticket_3_1);

        assert_eq!(rate_limiter.inner.lock().unwrap().semaphore.available_permits(), 4);
        assert_eq!(rate_limiter.inner.lock().unwrap().active_connections.len(), 0);
    }

    #[tokio::test]
    async fn test_per_ip_limits_remain_enforced() {
        let user_1 = IpAddr::from_str("127.0.0.1").unwrap();
        let user_2 = IpAddr::from_str("127.0.0.2").unwrap();

        let rate_limiter = Arc::new(InMemoryRateLimit::new(5, 2));

        let ticket_1_1 = Arc::clone(&rate_limiter).try_acquire(user_1).unwrap();
        let ticket_1_2 = Arc::clone(&rate_limiter).try_acquire(user_1).unwrap();

        let result = Arc::clone(&rate_limiter).try_acquire(user_1);
        assert!(result.is_err());
        assert_eq!(result.err().unwrap().to_string(), "Rate Limit Reached: IP limit exceeded");

        let ticket_2_1 = Arc::clone(&rate_limiter).try_acquire(user_2).unwrap();
        drop(ticket_1_1);

        let ticket_1_3 = Arc::clone(&rate_limiter).try_acquire(user_1).unwrap();

        let result = Arc::clone(&rate_limiter).try_acquire(user_1);
        assert!(result.is_err());
        assert_eq!(result.err().unwrap().to_string(), "Rate Limit Reached: IP limit exceeded");

        drop(ticket_1_2);
        drop(ticket_1_3);
        drop(ticket_2_1);

        assert_eq!(rate_limiter.inner.lock().unwrap().semaphore.available_permits(), 5);
        assert_eq!(rate_limiter.inner.lock().unwrap().active_connections.len(), 0);
    }
}
