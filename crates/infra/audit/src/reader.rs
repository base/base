use anyhow::Result;
use async_trait::async_trait;

use crate::types::BundleEvent;

/// A bundle event with metadata.
#[derive(Debug, Clone)]
pub struct Event {
    /// The event key.
    pub key: String,
    /// The bundle event.
    pub event: BundleEvent,
    /// The event timestamp in milliseconds.
    pub timestamp: i64,
}

/// Trait for reading bundle events.
#[async_trait]
pub trait EventReader {
    /// Reads the next event.
    async fn read_event(&mut self) -> Result<Event>;
    /// Commits the last read message.
    async fn commit(&mut self) -> Result<()>;
}
