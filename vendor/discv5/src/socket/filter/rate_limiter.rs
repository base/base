use std::{
    convert::TryInto,
    hash::Hash,
    net::IpAddr,
    time::{Duration, Instant},
};

use enr::NodeId;
use fnv::FnvHashMap;

/// Nanoseconds since a given time.
// Maintained as u64 to reduce footprint
// NOTE: this also implies that the rate limiter will manage checking if a batch is allowed for at
//       most <init time> + u64::MAX nanosecs, ~500 years. So it is realistic to assume this is fine.
type Nanosecs = u64;

/// User-friendly rate limiting parameters of the GCRA.
///
/// A quota of `max_tokens` tokens every `replenish_all_every` units of time means that:
/// 1. One token is replenished every `replenish_all_every`/`max_tokens` units of time.
/// 2. Instantaneous bursts (batches) of up to `max_tokens` tokens are allowed.
///
/// The above implies that if `max_tokens` is greater than 1, the perceived rate may be higher (but
/// bounded) than the defined rate when instantaneous bursts occur. For instance, for a rate of
/// 4T/2s a first burst of 4T is allowed with subsequent requests of 1T every 0.5s forever,
/// producing a perceived rate over the window of the first 2s of 8T. However, subsequent sliding
/// windows of 2s keep the limit.
///
/// In this scenario using the same rate as above, the sender is always maxing out their tokens,
/// except at seconds 1.5, 3, 3.5 and 4
///
/// ```ignore
///            x
///      used  x
///    tokens  x           x           x
///      at a  x  x  x     x  x        x
///     given  +--+--+--o--+--+--o--o--o--> seconds
///      time  |  |  |  |  |  |  |  |  |
///            0     1     2     3     4
///
///            4  1  1  1  2  1  1  2  3 <= available tokens when the batch is received
/// ```
///
/// For a sender to request a batch of `n`T, they would need to wait at least
/// n*`replenish_all_every`/`max_tokens` units of time since their last request.
///
/// To produce hard limits, set `max_tokens` to 1.
#[derive(Clone)]
pub struct Quota {
    /// How often are `max_tokens` fully replenished.
    replenish_all_every: Duration,
    /// Token limit. This translates on how large can an instantaneous batch of
    /// tokens be.
    max_tokens: u64,
}

/// Manages rate limiting of requests per peer, with differentiated rates per protocol.
#[derive(Debug, Clone)]
pub struct RateLimiter {
    /// An estimate of the maximum requests per second. This is only used for estimating the size
    /// of the cache for measuring metrics
    total_requests_per_second: f32,
    /// Creation time of the rate limiter.
    init_time: Instant,
    /// Total rate limit. Must be set.
    total_rl: Limiter<()>,
    /// Rate limit for each node
    node_rl: Option<Limiter<NodeId>>,
    /// Rate limit for each ip.
    ip_rl: Option<Limiter<IpAddr>>,
}

/// Error type for non conformant requests
pub enum RateLimitedErr {
    /// Required tokens for this request exceed the maximum
    TooLarge,
    /// Request does not fit in the quota. Gives the earliest time the request could be accepted.
    TooSoon(Duration),
}

pub enum LimitKind {
    /// Request counts towards the total limit.
    Total,
    /// Request counts towards the NodeId limit.
    NodeId(NodeId),
    /// Request counts toward the ip limit.
    Ip(IpAddr),
}

/// User-friendly builder of a `RateLimiter`. The user can specify three kinds of rate limits but
/// must at least set the total quota. The three types are:
/// 1. Total Quota - Specifies the total number of inbound requests. This must be set.
/// 2. Node Quota - Specifies the number of requests per node id.
/// 3. IP Quota - Specifies the number of requests per IP.
///
/// Quotas can be set via the X_one_every() functions to set hard limits as described above. Using
/// the `X_n_every()` functions allow for bursts.
#[derive(Default)]
pub struct RateLimiterBuilder {
    /// Quota for total received RPCs.
    total_quota: Option<Quota>,
    /// Quota for each node-id.
    node_quota: Option<Quota>,
    /// Quota for each IP.
    ip_quota: Option<Quota>,
}

#[allow(dead_code)]
impl RateLimiterBuilder {
    /// Get an empty `RateLimiterBuilder`.
    pub fn new() -> Self {
        Default::default()
    }

    /// Set the total quota.
    fn total_quota(mut self, quota: Quota) -> Self {
        self.total_quota = Some(quota);
        self
    }

    /// Set the node quota.
    fn node_quota(mut self, quota: Quota) -> Self {
        self.node_quota = Some(quota);
        self
    }

    /// Set the IP quota.
    fn ip_quota(mut self, quota: Quota) -> Self {
        self.ip_quota = Some(quota);
        self
    }

    /// Allow one token every `time_period` to be used for the total RPC limit.
    /// This produces a hard limit.
    pub fn total_one_every(self, time_period: Duration) -> Self {
        self.total_quota(Quota { replenish_all_every: time_period, max_tokens: 1 })
    }

    /// Allow one token every `time_period` to be used for the node RPC limit.
    /// This produces a hard limit.
    pub fn node_one_every(self, time_period: Duration) -> Self {
        self.node_quota(Quota { replenish_all_every: time_period, max_tokens: 1 })
    }

    /// Allow one token every `time_period` to be used for the total RPC limit per IP.
    /// This produces a hard limit.
    pub fn ip_one_every(self, time_period: Duration) -> Self {
        self.ip_quota(Quota { replenish_all_every: time_period, max_tokens: 1 })
    }

    /// Allow `n` tokens to be use used every `time_period` for the total.
    pub fn total_n_every(self, n: u64, time_period: Duration) -> Self {
        self.total_quota(Quota { replenish_all_every: time_period, max_tokens: n })
    }

    /// Allow `n` tokens to be use used every `time_period` for the total.
    pub fn node_n_every(self, n: u64, time_period: Duration) -> Self {
        self.node_quota(Quota { replenish_all_every: time_period, max_tokens: n })
    }

    /// Allow `n` tokens to be use used every `time_period` for the total.
    pub fn ip_n_every(self, n: u64, time_period: Duration) -> Self {
        self.ip_quota(Quota { replenish_all_every: time_period, max_tokens: n })
    }

    pub fn build(self) -> Result<RateLimiter, &'static str> {
        // get our quotas
        let total_quota = self.total_quota.ok_or("Total quota not specified and must be set.")?;

        // create the rate limiters
        let total_rl = Limiter::from_quota(total_quota.clone())?;
        let node_rl = match self.node_quota {
            Some(q) => Some(Limiter::from_quota(q)?),
            None => None,
        };
        let ip_rl = match self.ip_quota {
            Some(q) => Some(Limiter::from_quota(q)?),
            None => None,
        };

        let total_requests_per_second = if total_quota.max_tokens == 1 {
            (1.0 / total_quota.replenish_all_every.as_secs_f32()
                / Duration::from_secs(1).as_secs_f32())
            .round()
        } else {
            (2.0 * total_quota.max_tokens as f32 // multiply by 2 to account for potential bursts
                / total_quota.replenish_all_every.as_secs_f32()
                / Duration::from_secs(1).as_secs_f32())
            .round()
        };

        Ok(RateLimiter {
            total_requests_per_second,
            total_rl,
            node_rl,
            ip_rl,
            init_time: Instant::now(),
        })
    }
}

impl RateLimiter {
    /// Indicates whether the request is allowed based on the configured rate limits.
    pub fn allows(&mut self, request: &LimitKind) -> Result<(), RateLimitedErr> {
        let time_since_start = self.init_time.elapsed();
        let tokens = 1; // Only count each of these as one.

        // Check the limits
        match request {
            LimitKind::Total => self.total_rl.allows(time_since_start, &(), tokens),
            LimitKind::Ip(ip_addr) => {
                if let Some(limiter) = self.ip_rl.as_mut() {
                    limiter.allows(time_since_start, ip_addr, tokens)
                } else {
                    Ok(())
                }
            }
            LimitKind::NodeId(node_id) => {
                if let Some(limiter) = self.node_rl.as_mut() {
                    limiter.allows(time_since_start, node_id, tokens)
                } else {
                    Ok(())
                }
            }
        }
    }

    /// Returns the expected total requests per second.
    pub fn total_requests_per_second(&self) -> f32 {
        self.total_requests_per_second
    }

    /// Prunes excess entries. Should be called regularly (30 seconds) to remove old entries.
    pub fn prune(&mut self) {
        let time_since_start = self.init_time.elapsed();
        self.total_rl.prune(time_since_start);
        if let Some(v) = self.ip_rl.as_mut() {
            v.prune(time_since_start)
        };
        if let Some(v) = self.node_rl.as_mut() {
            v.prune(time_since_start)
        };
    }
}

/// Per key rate limiter using the token bucket / leaky bucket as a meter rate limiting algorithm,
/// with the GCRA implementation.
#[derive(Debug, Clone)]
pub struct Limiter<Key: Hash + Eq + Clone> {
    /// After how long is the bucket considered full via replenishing 1T every `t`.
    tau: Nanosecs,
    /// How often is 1T replenished.
    t: Nanosecs,
    /// Time when the bucket will be full for each peer. TAT (theoretical arrival time) from GCRA.
    tat_per_key: FnvHashMap<Key, Nanosecs>,
}

impl<Key: Hash + Eq + Clone> Limiter<Key> {
    pub fn from_quota(quota: Quota) -> Result<Self, &'static str> {
        if quota.max_tokens == 0 {
            return Err("Max number of tokens should be positive");
        }
        let tau = quota.replenish_all_every.as_nanos();
        if tau == 0 {
            return Err("Replenish time must be positive");
        }
        let t = (tau / quota.max_tokens as u128)
            .try_into()
            .map_err(|_| "total replenish time is too long")?;
        let tau = tau.try_into().map_err(|_| "total replenish time is too long")?;
        Ok(Limiter { tau, t, tat_per_key: FnvHashMap::default() })
    }

    pub fn allows(
        &mut self,
        time_since_start: Duration,
        key: &Key,
        tokens: u64,
    ) -> Result<(), RateLimitedErr> {
        let time_since_start = time_since_start.as_nanos() as u64;
        let tau = self.tau;
        let t = self.t;
        // how long does it take to replenish these tokens
        let additional_time = t * tokens;
        if additional_time > tau {
            // the time required to process this amount of tokens is longer than the time that
            // makes the bucket full. So, this batch can _never_ be processed
            return Err(RateLimitedErr::TooLarge);
        }
        // If the key is new, we consider their bucket full (which means, their request will be
        // allowed)
        let tat = self.tat_per_key.entry(key.clone()).or_insert(time_since_start);
        // check how soon could the request be made
        let earliest_time = (*tat + additional_time).saturating_sub(tau);
        // earliest_time is in the future
        if time_since_start < earliest_time {
            Err(RateLimitedErr::TooSoon(Duration::from_nanos(
                /* time they need to wait, i.e. how soon were they */
                earliest_time - time_since_start,
            )))
        } else {
            // calculate the new TAT
            *tat = time_since_start.max(*tat) + additional_time;
            Ok(())
        }
    }

    /// Removes keys for which their bucket is full by `time_limit`
    pub fn prune(&mut self, time_limit: Duration) {
        let lim = &mut (time_limit.as_nanos() as u64);
        // remove those for which tat < lim
        self.tat_per_key.retain(|_k, tat| tat >= lim)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{Limiter, Quota};

    #[test]
    fn it_works_a() {
        let mut limiter = Limiter::from_quota(Quota {
            replenish_all_every: Duration::from_secs(2),
            max_tokens: 4,
        })
        .unwrap();
        let key = 10;
        //        x
        //  used  x
        // tokens x           x
        //        x  x  x     x
        //        +--+--+--+--+----> seconds
        //        |  |  |  |  |
        //        0     1     2

        assert!(limiter.allows(Duration::from_secs_f32(0.0), &key, 4).is_ok());
        limiter.prune(Duration::from_secs_f32(0.1));
        assert!(limiter.allows(Duration::from_secs_f32(0.1), &key, 1).is_err());
        assert!(limiter.allows(Duration::from_secs_f32(0.5), &key, 1).is_ok());
        assert!(limiter.allows(Duration::from_secs_f32(1.0), &key, 1).is_ok());
        assert!(limiter.allows(Duration::from_secs_f32(1.4), &key, 1).is_err());
        assert!(limiter.allows(Duration::from_secs_f32(2.0), &key, 2).is_ok());
    }

    #[test]
    fn it_works_b() {
        let mut limiter = Limiter::from_quota(Quota {
            replenish_all_every: Duration::from_secs(2),
            max_tokens: 4,
        })
        .unwrap();
        let key = 10;
        // if we limit to 4T per 2s, check that 4 requests worth 1 token can be sent before the
        // first half second, when one token will be available again. Check also that before
        // regaining a token, another request is rejected

        assert!(limiter.allows(Duration::from_secs_f32(0.0), &key, 1).is_ok());
        assert!(limiter.allows(Duration::from_secs_f32(0.1), &key, 1).is_ok());
        assert!(limiter.allows(Duration::from_secs_f32(0.2), &key, 1).is_ok());
        assert!(limiter.allows(Duration::from_secs_f32(0.3), &key, 1).is_ok());
        assert!(limiter.allows(Duration::from_secs_f32(0.4), &key, 1).is_err());
    }
}
