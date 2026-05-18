use std::time::Duration;

/// Configuration for the transaction pool consumer task.
///
/// The consumer continuously reads from the pool's `best_transactions()` iterator,
/// deduplicates transactions, and broadcasts them for downstream forwarding to
/// builder nodes.
#[derive(Debug, Clone)]
pub struct ConsumerConfig {
    /// Duration after which a previously sent transaction may be re-sent.
    ///
    /// Transactions seen within this window are skipped to avoid sending
    /// duplicates to the forwarder.
    pub resend_after: Duration,

    /// Bounded channel capacity for outgoing transactions.
    pub channel_capacity: usize,

    /// Sleep duration when the pool iterator yields no transactions,
    /// preventing busy-spinning.
    pub poll_interval: Duration,
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

impl ConsumerConfig {
    /// Sets the resend-after duration.
    pub const fn with_resend_after(mut self, duration: Duration) -> Self {
        self.resend_after = duration;
        self
    }

    /// Sets the channel capacity.
    pub const fn with_channel_capacity(mut self, capacity: usize) -> Self {
        self.channel_capacity = capacity;
        self
    }

    /// Sets the poll interval.
    pub const fn with_poll_interval(mut self, interval: Duration) -> Self {
        self.poll_interval = interval;
        self
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

    #[test]
    fn builder_methods() {
        let config = ConsumerConfig::default()
            .with_resend_after(Duration::from_secs(10))
            .with_channel_capacity(5_000)
            .with_poll_interval(Duration::from_millis(5));

        assert_eq!(config.resend_after, Duration::from_secs(10));
        assert_eq!(config.channel_capacity, 5_000);
        assert_eq!(config.poll_interval, Duration::from_millis(5));
    }
}
