//! Per-thread accounting of time spent emitting transaction events.

use std::{cell::Cell, time::Duration};

thread_local! {
    static ATTEMPTS: Cell<u64> = const { Cell::new(0) };
    static NANOS: Cell<u64> = const { Cell::new(0) };
}

/// Cumulative emission work recorded on one thread.
///
/// Attempts count every event that reached construction while a writer was
/// configured, whether or not the writer accepted it. Duration is the time the
/// emitting thread spent building, validating, serializing, and enqueueing those
/// events.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TransactionEventEmissionTotals {
    /// Events that reached construction.
    pub attempts: u64,
    /// Emitting-thread time spent on those events.
    pub duration: Duration,
}

impl TransactionEventEmissionTotals {
    /// Returns the work recorded between `earlier` and `self`.
    pub const fn since(self, earlier: Self) -> Self {
        Self {
            attempts: self.attempts.saturating_sub(earlier.attempts),
            duration: self.duration.saturating_sub(earlier.duration),
        }
    }
}

/// Per-thread emission accounting.
///
/// Recording touches only thread-local counters, so it adds no cross-thread
/// contention to hot paths. To attribute emission cost to a unit of work, take
/// a [`current_thread`](Self::current_thread) snapshot before and after it and
/// diff them with [`TransactionEventEmissionTotals::since`]. Both snapshots must
/// run on the same thread with no `.await` in between, because async tasks can
/// migrate between worker threads.
#[derive(Debug)]
pub struct TransactionEventEmissionStats;

impl TransactionEventEmissionStats {
    /// Returns cumulative totals for the calling thread.
    pub fn current_thread() -> TransactionEventEmissionTotals {
        TransactionEventEmissionTotals {
            attempts: ATTEMPTS.get(),
            duration: Duration::from_nanos(NANOS.get()),
        }
    }

    /// Adds one emission attempt that took `elapsed` on the calling thread.
    pub fn record(elapsed: Duration) {
        let nanos = u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX);
        ATTEMPTS.set(ATTEMPTS.get().saturating_add(1));
        NANOS.set(NANOS.get().saturating_add(nanos));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn since_reports_work_recorded_on_this_thread() {
        let before = TransactionEventEmissionStats::current_thread();

        TransactionEventEmissionStats::record(Duration::from_micros(3));
        TransactionEventEmissionStats::record(Duration::from_micros(4));

        let delta = TransactionEventEmissionStats::current_thread().since(before);
        assert_eq!(
            delta,
            TransactionEventEmissionTotals { attempts: 2, duration: Duration::from_micros(7) }
        );
    }

    #[test]
    fn other_threads_do_not_affect_this_thread() {
        let before = TransactionEventEmissionStats::current_thread();

        std::thread::spawn(|| TransactionEventEmissionStats::record(Duration::from_micros(5)))
            .join()
            .unwrap();

        let delta = TransactionEventEmissionStats::current_thread().since(before);
        assert_eq!(delta, TransactionEventEmissionTotals::default());
    }

    #[test]
    fn since_saturates_when_earlier_is_ahead() {
        let later =
            TransactionEventEmissionTotals { attempts: 1, duration: Duration::from_nanos(1) };
        let earlier =
            TransactionEventEmissionTotals { attempts: 5, duration: Duration::from_nanos(9) };

        assert_eq!(later.since(earlier), TransactionEventEmissionTotals::default());
    }
}
