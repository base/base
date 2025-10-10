use chrono::{DateTime, Local};
use std::fmt::{self, Display};
use std::time::Instant;

/// Types of transaction events to track
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TxEvent {
    Dropped,
    Replaced,
    Pending,
    Queued,
    BlockInclusion,
    PendingToQueued,
    QueuedToPending,
    Overflowed,
}

impl Display for TxEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            TxEvent::Dropped => "dropped",
            TxEvent::Replaced => "replaced",
            TxEvent::Pending => "pending",
            TxEvent::Queued => "queued",
            TxEvent::BlockInclusion => "block_inclusion",
            TxEvent::PendingToQueued => "pending_to_queued",
            TxEvent::QueuedToPending => "queued_to_pending",
            TxEvent::Overflowed => "overflowed",
        };
        write!(f, "{s}")
    }
}

/// Types of pools a transaction can be in
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Pool {
    Pending,
    Queued,
}

/// History of events for a transaction
pub struct EventLog {
    pub mempool_time: Instant,
    pub events: Vec<(DateTime<Local>, TxEvent)>,
    pub limit: usize,
}

impl EventLog {
    pub fn new(t: DateTime<Local>, event: TxEvent) -> Self {
        Self {
            mempool_time: Instant::now(),
            events: vec![(t, event)],
            limit: 10,
        }
    }

    pub fn push(&mut self, t: DateTime<Local>, event: TxEvent) {
        self.events.push((t, event));
        self.limit += 1;
    }

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
