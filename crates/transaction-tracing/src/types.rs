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
