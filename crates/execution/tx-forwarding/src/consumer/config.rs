use std::time::Duration;

/// Configuration for a transaction pool consumer task.
///
/// The consumer continuously reads from the pool's `best_transactions()` iterator,
/// deduplicates transactions for one destination, and queues them for forwarding.
#[derive(Debug, Clone)]
pub(crate) struct ConsumerConfig {
    /// Duration after which a previously sent transaction may be re-sent.
    ///
    /// Transactions seen within this window are skipped to avoid sending
    /// duplicates to the forwarder.
    pub(crate) resend_after: Duration,

    /// Bounded per-destination queue capacity for outgoing transactions.
    pub(crate) channel_capacity: usize,

    /// Sleep duration when the pool iterator yields no transactions,
    /// preventing busy-spinning.
    pub(crate) poll_interval: Duration,
}

impl Default for ConsumerConfig {
    fn default() -> Self {
        Self {
            resend_after: Duration::from_secs(4),
            channel_capacity: 10_000,
            poll_interval: Duration::from_millis(10),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults() {
        let config = ConsumerConfig::default();
        assert_eq!(config.resend_after, Duration::from_secs(4));
        assert_eq!(config.channel_capacity, 10_000);
        assert_eq!(config.poll_interval, Duration::from_millis(10));
    }
}
