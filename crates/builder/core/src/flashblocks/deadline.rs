//! Payload job deadline calculation and timer construction.

use core::{pin::Pin, time::Duration};
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::time::Sleep;
use tracing::warn;

/// Calculates how long a payload job remains available and constructs its deadline timer.
///
/// The deadline is critical for payload availability: once reached, the payload job stops and
/// cannot be queried again. Tight deadlines near the block timestamp risk expiring before the node
/// requests the payload, so this type owns an additional configured leeway.
///
/// The leeway also accommodates bursts caused by long batcher channel durations. A batcher update
/// can trigger hundreds of forkchoice updates and block queries at once, delaying the subsequent
/// `getPayload` request by several seconds. Retaining payloads beyond their nominal timestamp avoids
/// losing blocks in that situation.
///
/// A longer-term alternative would be cancellation logic that retires existing jobs when new block
/// building requests arrive.
#[derive(Debug, Clone, Copy)]
pub struct PayloadJobDeadline {
    extra_block_deadline: Duration,
}

impl PayloadJobDeadline {
    /// Creates a deadline calculator that owns the configured additional payload retention time.
    pub const fn new(extra_block_deadline: Duration) -> Self {
        Self { extra_block_deadline }
    }

    /// Calculates the duration until the attributes timestamp plus the configured leeway.
    ///
    /// Timestamps at or before the current time receive a minimum one-second base duration. If the
    /// system clock predates the Unix epoch, only the configured leeway is returned.
    pub fn calculate(&self, attributes_timestamp: u64) -> Duration {
        let unix_now = match SystemTime::now().duration_since(UNIX_EPOCH) {
            Ok(duration) => duration.as_secs(),
            Err(error) => {
                warn!(error = %error, "System clock went backward, using configured deadline leeway");
                return self.extra_block_deadline;
            }
        };

        let duration_until = attributes_timestamp.saturating_sub(unix_now);
        let duration_until = if duration_until == 0 {
            Duration::from_secs(1)
        } else {
            Duration::from_secs(duration_until)
        };

        duration_until + self.extra_block_deadline
    }

    /// Returns a pinned timer that completes when the calculated deadline is reached.
    pub fn sleep(&self, attributes_timestamp: u64) -> Pin<Box<Sleep>> {
        Box::pin(tokio::time::sleep(self.calculate(attributes_timestamp)))
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use rstest::rstest;

    use super::PayloadJobDeadline;

    #[rstest]
    #[case(Duration::ZERO, Duration::from_secs(1))]
    #[case(Duration::from_secs(5), Duration::from_secs(6))]
    fn calculate_applies_minimum_duration_and_leeway(
        #[case] extra_block_deadline: Duration,
        #[case] expected: Duration,
    ) {
        let deadline = PayloadJobDeadline::new(extra_block_deadline);

        assert_eq!(deadline.calculate(0), expected);
    }

    #[rstest]
    #[case(Duration::ZERO)]
    #[case(Duration::from_secs(5))]
    fn calculate_uses_attributes_timestamp(#[case] extra_block_deadline: Duration) {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        let deadline = PayloadJobDeadline::new(extra_block_deadline);

        let calculated = deadline.calculate(now + 60);

        assert!(calculated >= Duration::from_secs(59) + extra_block_deadline);
        assert!(calculated <= Duration::from_secs(60) + extra_block_deadline);
    }
}
