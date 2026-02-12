use anyhow::Result;
use async_trait::async_trait;
use rdkafka::producer::{FutureProducer, FutureRecord};
use tracing::{debug, error, info};

use crate::types::BundleEvent;

/// Trait for publishing bundle events.
#[async_trait]
pub trait BundleEventPublisher: Send + Sync {
    /// Publishes a single bundle event.
    async fn publish(&self, event: BundleEvent) -> Result<()>;

    /// Publishes multiple bundle events.
    async fn publish_all(&self, events: Vec<BundleEvent>) -> Result<()>;
}

/// Publishes bundle events to Kafka.
#[derive(Clone)]
pub struct KafkaBundleEventPublisher {
    producer: FutureProducer,
    topic: String,
}

impl std::fmt::Debug for KafkaBundleEventPublisher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KafkaBundleEventPublisher")
            .field("topic", &self.topic)
            .finish_non_exhaustive()
    }
}

impl KafkaBundleEventPublisher {
    /// Creates a new Kafka bundle event publisher.
    pub const fn new(producer: FutureProducer, topic: String) -> Self {
        Self { producer, topic }
    }

    async fn send_event(&self, event: &BundleEvent) -> Result<()> {
        let bundle_id = event.bundle_id();
        let key = event.generate_event_key();
        let payload = serde_json::to_vec(event)?;

        let record = FutureRecord::to(&self.topic).key(&key).payload(&payload);

        match self.producer.send(record, tokio::time::Duration::from_secs(5)).await {
            Ok(_) => {
                debug!(
                    bundle_id = %bundle_id,
                    topic = %self.topic,
                    payload_size = payload.len(),
                    "successfully published event"
                );
                Ok(())
            }
            Err((err, _)) => {
                error!(
                    bundle_id = %bundle_id,
                    topic = %self.topic,
                    error = %err,
                    "failed to publish event"
                );
                Err(anyhow::anyhow!("Failed to publish event: {err}"))
            }
        }
    }
}

#[async_trait]
impl BundleEventPublisher for KafkaBundleEventPublisher {
    async fn publish(&self, event: BundleEvent) -> Result<()> {
        self.send_event(&event).await
    }

    async fn publish_all(&self, events: Vec<BundleEvent>) -> Result<()> {
        for event in events {
            self.send_event(&event).await?;
        }
        Ok(())
    }
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
