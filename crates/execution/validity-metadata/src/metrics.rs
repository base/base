//! Metrics for the validity metadata recorder and status lookups.

base_metrics::define_metrics! {
    builder.validity_metadata,
    struct = ValidityMetadataMetrics,
    #[describe("Block records accepted into the validity metadata write queue")]
    records_queued: counter,
    #[describe("Block records lost before reaching durable storage, labeled by cause")]
    #[label(name = "reason", default = ["queue_full", "closed", "invalid", "write_failed"])]
    records_dropped: counter,
    #[describe("Block records durably committed, labeled by whether they were new or identical retries")]
    #[label(name = "outcome", default = ["inserted", "unchanged"])]
    records_written: counter,
    #[describe("Block records whose block hash has conflicting classifications and is answered with an error")]
    records_conflicting: counter,
    #[describe("base_getValidityTransactionStatus lookups answered with an internal error, labeled by cause")]
    #[label(name = "reason", default = ["provider", "transaction_mismatch", "storage", "missing_body", "inconsistent", "task"])]
    status_lookup_failures: counter,
}

impl ValidityMetadataMetrics {
    /// Returns whether a log should be emitted when an event count moves from `previous` to
    /// `current`: the first occurrence and every power-of-two occurrence after it are logged, so
    /// repeated failures produce logarithmically many lines while metrics count every event.
    pub const fn is_log_sampled(previous: u64, current: u64) -> bool {
        match previous.saturating_add(1).checked_next_power_of_two() {
            Some(next) => current > previous && next <= current,
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logs_first_and_power_of_two_occurrences() {
        let sampled: Vec<u64> =
            (1..=100).filter(|n| ValidityMetadataMetrics::is_log_sampled(n - 1, *n)).collect();
        assert_eq!(sampled, vec![1, 2, 4, 8, 16, 32, 64]);
    }

    #[test]
    fn batch_increments_log_when_crossing_a_power_of_two() {
        assert!(ValidityMetadataMetrics::is_log_sampled(0, 5));
        assert!(ValidityMetadataMetrics::is_log_sampled(5, 8));
        assert!(!ValidityMetadataMetrics::is_log_sampled(8, 15));
        assert!(ValidityMetadataMetrics::is_log_sampled(15, 16));
        assert!(!ValidityMetadataMetrics::is_log_sampled(7, 7));
        assert!(!ValidityMetadataMetrics::is_log_sampled(u64::MAX - 1, u64::MAX));
    }
}
