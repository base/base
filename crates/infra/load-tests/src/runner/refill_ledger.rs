//! Acknowledgement tracking for refill cycles.

use std::{
    collections::VecDeque,
    ops::Range,
    time::{Duration, Instant},
};

/// Whether a refill cycle is still waiting for its batches to be acknowledged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefillWait {
    /// The cycle is waiting synchronously; its lag is collected when the wait ends.
    Waiting,
    /// The cycle stopped waiting; its lag is reported when its last batch completes.
    Late,
}

/// In-progress refill cycle tracked by [`RefillLedger`].
#[derive(Debug)]
pub struct OpenRefill {
    batches: Range<u64>,
    outstanding: u64,
    started_at: Instant,
    latest: Instant,
    wait: RefillWait,
}

/// Outstanding submit batches of each refill cycle.
///
/// Batch ids are assigned contiguously, so each cycle owns one id range and needs only a
/// counter rather than a per-batch completion record.
#[derive(Debug, Default)]
pub struct RefillLedger {
    open: VecDeque<OpenRefill>,
}

impl RefillLedger {
    /// Starts tracking a cycle's batches. Ranges must be opened in ascending order.
    pub fn open(&mut self, batches: Range<u64>, started_at: Instant) {
        if batches.is_empty() {
            return;
        }
        self.open.push_back(OpenRefill {
            outstanding: batches.end - batches.start,
            batches,
            started_at,
            latest: started_at,
            wait: RefillWait::Waiting,
        });
    }

    /// Records a completed batch, returning the acknowledgement lag of a late cycle it finished.
    pub fn complete(&mut self, id: u64, completed_at: Instant) -> Option<Duration> {
        let index = self.open.partition_point(|refill| refill.batches.end <= id);
        let refill = self.open.get_mut(index).filter(|refill| refill.batches.contains(&id))?;
        refill.outstanding = refill.outstanding.saturating_sub(1);
        refill.latest = refill.latest.max(completed_at);
        if refill.outstanding > 0 || refill.wait == RefillWait::Waiting {
            return None;
        }
        let refill = self.open.remove(index)?;
        Some(refill.latest.saturating_duration_since(refill.started_at))
    }

    /// Returns whether every batch in a waiting cycle has completed.
    pub fn is_acked(&self, batches: &Range<u64>) -> bool {
        self.find(batches).is_none_or(|index| self.open[index].outstanding == 0)
    }

    /// Ends a cycle's synchronous wait. Returns its lag when fully acknowledged; otherwise the
    /// cycle turns late and its lag is returned by [`Self::complete`].
    pub fn stop_waiting(&mut self, batches: &Range<u64>) -> Option<Duration> {
        let Some(index) = self.find(batches) else {
            return Some(Duration::ZERO);
        };
        if self.open[index].outstanding > 0 {
            self.open[index].wait = RefillWait::Late;
            return None;
        }
        let refill = self.open.remove(index)?;
        Some(refill.latest.saturating_duration_since(refill.started_at))
    }

    /// Drops an un-sent batch from its cycle without recording a completion lag.
    pub fn abandon(&mut self, id: u64) {
        let index = self.open.partition_point(|refill| refill.batches.end <= id);
        let Some(refill) = self.open.get_mut(index).filter(|refill| refill.batches.contains(&id))
        else {
            return;
        };
        refill.outstanding = refill.outstanding.saturating_sub(1);
        if refill.outstanding == 0 && refill.wait == RefillWait::Late {
            self.open.remove(index);
        }
    }

    fn find(&self, batches: &Range<u64>) -> Option<usize> {
        let index = self.open.partition_point(|refill| refill.batches.start < batches.start);
        self.open.get(index).filter(|refill| refill.batches == *batches).map(|_| index)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acknowledged_cycle_reports_latest_completion() {
        let mut ledger = RefillLedger::default();
        let started_at = Instant::now();
        ledger.open(10..12, started_at);

        assert_eq!(ledger.complete(11, started_at + Duration::from_millis(40)), None);
        assert!(!ledger.is_acked(&(10..12)));
        assert_eq!(ledger.complete(10, started_at + Duration::from_millis(30)), None);
        assert!(ledger.is_acked(&(10..12)));

        assert_eq!(ledger.stop_waiting(&(10..12)), Some(Duration::from_millis(40)));
    }

    #[test]
    fn late_cycle_reports_lag_when_its_last_batch_completes() {
        let mut ledger = RefillLedger::default();
        let started_at = Instant::now();
        ledger.open(10..12, started_at);
        ledger.open(12..13, started_at);

        assert_eq!(ledger.stop_waiting(&(10..12)), None);
        assert_eq!(ledger.complete(10, started_at + Duration::from_millis(120)), None);
        assert_eq!(ledger.complete(12, started_at + Duration::from_millis(130)), None);
        assert_eq!(
            ledger.complete(11, started_at + Duration::from_millis(145)),
            Some(Duration::from_millis(145))
        );
        assert_eq!(ledger.stop_waiting(&(12..13)), Some(Duration::from_millis(130)));
    }

    #[test]
    fn abandoned_batches_release_their_cycle() {
        let mut ledger = RefillLedger::default();
        let started_at = Instant::now();
        ledger.open(10..12, started_at);

        ledger.abandon(11);
        assert!(!ledger.is_acked(&(10..12)));
        assert_eq!(ledger.complete(10, started_at + Duration::from_millis(20)), None);
        assert!(ledger.is_acked(&(10..12)));
        assert_eq!(ledger.stop_waiting(&(10..12)), Some(Duration::from_millis(20)));

        ledger.open(12..13, started_at);
        assert_eq!(ledger.stop_waiting(&(12..13)), None);
        ledger.abandon(12);
        assert_eq!(ledger.stop_waiting(&(12..13)), Some(Duration::ZERO), "late cycle is removed");
    }

    #[test]
    fn untracked_batches_are_ignored() {
        let mut ledger = RefillLedger::default();
        let started_at = Instant::now();
        ledger.open(5..6, started_at);

        assert_eq!(ledger.complete(4, started_at), None, "prefill batches have no cycle");
        assert_eq!(ledger.complete(6, started_at), None);
        assert!(!ledger.is_acked(&(5..6)));
        assert_eq!(ledger.stop_waiting(&(7..7)), Some(Duration::ZERO), "empty cycles are acked");
    }
}
