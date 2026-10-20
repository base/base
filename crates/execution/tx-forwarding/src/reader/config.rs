use std::time::Duration;

/// Configuration for a transaction pool reader task.
///
/// The reader continuously reads from the pool's `best_transactions()` iterator,
/// deduplicates transactions for one destination, and queues them for forwarding.
#[derive(Debug, Clone)]
pub(crate) struct ReaderConfig {
    /// Duration after which a previously sent transaction may be re-sent.
    ///
    /// Transactions seen within this window are skipped to avoid sending
    /// duplicates to the forwarder.
    pub(crate) resend_after: Duration,

    /// Bounded per-destination queue capacity for outgoing transactions.
    ///
    /// A transaction's position is fixed once it is queued, so a deep queue delays the point
    /// where lane ordering applies.
    pub(crate) channel_capacity: usize,

    /// Sleep duration when the pool iterator yields no transactions,
    /// preventing busy-spinning.
    pub(crate) poll_interval: Duration,

    /// Percentage of picks served oldest-first; the rest go to the highest bid.
    pub(crate) fifo_percent: u8,
}

impl ReaderConfig {
    /// Sleep between passes that found nothing new to send.
    pub(crate) const POLL_INTERVAL: Duration = Duration::from_millis(10);
}
