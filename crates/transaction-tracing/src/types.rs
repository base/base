use std::time::Instant;

use chrono::{DateTime, Local};
use derive_more::Display;

/// Types of transaction events to track
#[derive(Debug, Display, Clone, Copy, PartialEq)]
pub(crate) enum TxEvent {
    #[display("dropped")]
    Dropped,
    #[display("replaced")]
    Replaced,
    #[display("pending")]
    Pending,
    #[display("queued")]
    Queued,
    #[display("block_inclusion")]
    BlockInclusion,
    #[display("pending_to_queued")]
    PendingToQueued,
    #[display("queued_to_pending")]
    QueuedToPending,
    #[display("overflowed")]
    Overflowed,
}

/// Types of pools a transaction can be in
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Pool {
    Pending,
    Queued,
}

/// History of events for a transaction
pub(crate) struct EventLog {
    pub(crate) mempool_time: Instant,
    pub(crate) events: Vec<(DateTime<Local>, TxEvent)>,
    pub(crate) limit: usize,
}

impl EventLog {
    pub(crate) fn new(t: DateTime<Local>, event: TxEvent) -> Self {
        Self { mempool_time: Instant::now(), events: vec![(t, event)], limit: 10 }
    }

    pub(crate) fn push(&mut self, t: DateTime<Local>, event: TxEvent) {
        self.events.push((t, event));
    }

    pub(crate) fn to_vec(&self) -> Vec<String> {
        self.events
            .iter()
            .map(|(t, event)| {
                // example: 08:57:37.979 pm - Pending
                format!("{} - {}", t.format("%H:%M:%S%.3f"), event)
            })
            .collect::<Vec<_>>()
    }
}
