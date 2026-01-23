//! Domain types describing tracex events and pools.

use std::time::Instant;

use chrono::{DateTime, Local};
use derive_more::Display;
use serde::{Deserialize, Serialize};

/// Types of transaction events to track.
#[derive(Debug, Display, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TxEvent {
    /// Transaction dropped from the pool.
    #[display("dropped")]
    Dropped,
    /// Transaction replaced by a higher priced version.
    #[display("replaced")]
    Replaced,
    /// Transaction was observed in the pending pool.
    #[display("pending")]
    Pending,
    /// Transaction was queued due to dependencies (nonce gap, etc).
    #[display("queued")]
    Queued,
    /// Transaction included on chain.
    #[display("block_inclusion")]
    BlockInclusion,
    /// Transaction moved from pending -> queued.
    #[display("pending_to_queued")]
    PendingToQueued,
    /// Transaction moved from queued -> pending.
    #[display("queued_to_pending")]
    QueuedToPending,
    /// Transaction overflowed our tracking buffers.
    #[display("overflowed")]
    Overflowed,
}

/// Types of pools a transaction can be in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pool {
    /// Pending pool.
    Pending,
    /// Queued pool.
    Queued,
}

/// History of events for a transaction.
#[derive(Debug, Clone)]
pub struct EventLog {
    pub(crate) mempool_time: Instant,
    pub(crate) pending_time: Option<Instant>,
    pub(crate) events: Vec<(DateTime<Local>, TxEvent)>,
    pub(crate) limit: usize,
}

impl EventLog {
    /// Create a new log seeded with the first event.
    pub fn new(t: DateTime<Local>, event: TxEvent) -> Self {
        let now = Instant::now();
        let pending_time = if event == TxEvent::Pending { Some(now) } else { None };
        Self { mempool_time: now, pending_time, events: vec![(t, event)], limit: 10 }
    }

    /// Append a new `(timestamp, event)` tuple to the log.
    pub fn push(&mut self, t: DateTime<Local>, event: TxEvent) {
        self.events.push((t, event));
    }

    /// Render all events into human readable strings.
    pub fn to_vec(&self) -> Vec<String> {
        self.events
            .iter()
            .map(|(t, event)| {
                // example: 08:57:37.979 pm - Pending
                format!("{} - {}", t.format("%H:%M:%S%.3f"), event)
            })
            .collect::<Vec<_>>()
    }
}
