use anyhow::Result;
use async_trait::async_trait;
use tracing::info;

use crate::types::BundleEvent;

/// Trait for publishing bundle events.
#[async_trait]
pub trait BundleEventPublisher: Send + Sync {
    /// Publishes a single bundle event.
    async fn publish(&self, event: BundleEvent) -> Result<()>;

    /// Publishes multiple bundle events.
    async fn publish_all(&self, events: Vec<BundleEvent>) -> Result<()>;
}

/// Publishes bundle events to logs (for testing/debugging).
#[derive(Clone, Debug)]
pub struct LoggingBundleEventPublisher;

impl LoggingBundleEventPublisher {
    /// Creates a new logging bundle event publisher.
    pub const fn new() -> Self {
        Self
    }
}

impl Default for LoggingBundleEventPublisher {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl BundleEventPublisher for LoggingBundleEventPublisher {
    async fn publish(&self, event: BundleEvent) -> Result<()> {
        info!(
            bundle_id = %event.bundle_id(),
            event = ?event,
            "Received bundle event"
        );
        Ok(())
    }

    async fn publish_all(&self, events: Vec<BundleEvent>) -> Result<()> {
        for event in events {
            self.publish(event).await?;
        }
        Ok(())
    }
}
