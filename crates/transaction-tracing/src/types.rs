use std::{
    fmt::{self, Display},
    time::Instant,
};

use chrono::{DateTime, Local};

/// Types of transaction events to track
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum TxEvent {
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
            Self::Dropped => "dropped",
            Self::Replaced => "replaced",
            Self::Pending => "pending",
            Self::Queued => "queued",
            Self::BlockInclusion => "block_inclusion",
            Self::PendingToQueued => "pending_to_queued",
            Self::QueuedToPending => "queued_to_pending",
            Self::Overflowed => "overflowed",
        };
        write!(f, "{s}")
    }
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
