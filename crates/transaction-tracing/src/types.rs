use std::fmt::{self, Display};
use std::time::Instant;

/// Types of transaction events to track
#[derive(Debug, Clone, Copy, PartialEq)]
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
        write!(f, "{:?}", self)
    }
}

/// Types of pools a transaction can be in
#[derive(Debug, Clone, PartialEq)]
pub enum Pool {
    Pending,
    Queued,
}

/// History of events for a transaction
pub struct EventLog {
    pub mempool_time: Instant,
    pub events: Vec<TxEvent>,
    pub limit: usize,
}

impl EventLog {
    pub fn new(event: TxEvent) -> Self {
        Self {
            mempool_time: Instant::now(),
            events: vec![event],
            limit: 10,
        }
    }

    pub fn push(&mut self, event: TxEvent) {
        self.events.push(event);
        self.limit += 1;
    }

    pub fn to_string(&self) -> String {
        self.events
            .iter()
            .map(|event| event.to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }
}
