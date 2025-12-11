use crate::types::{BundleEvent, UserOpEvent};
use anyhow::Result;
use async_trait::async_trait;
use rdkafka::producer::{FutureProducer, FutureRecord};
use serde_json;
use tracing::{debug, error, info};

#[async_trait]
pub trait BundleEventPublisher: Send + Sync {
    async fn publish(&self, event: BundleEvent) -> Result<()>;

    async fn publish_all(&self, events: Vec<BundleEvent>) -> Result<()>;
}

#[derive(Clone)]
pub struct KafkaBundleEventPublisher {
    producer: FutureProducer,
    topic: String,
}

impl KafkaBundleEventPublisher {
    pub fn new(producer: FutureProducer, topic: String) -> Self {
        Self { producer, topic }
    }

    async fn send_event(&self, event: &BundleEvent) -> Result<()> {
        let bundle_id = event.bundle_id();
        let key = event.generate_event_key();
        let payload = serde_json::to_vec(event)?;

        let record = FutureRecord::to(&self.topic).key(&key).payload(&payload);

        match self
            .producer
            .send(record, tokio::time::Duration::from_secs(5))
            .await
        {
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

#[derive(Clone)]
pub struct LoggingBundleEventPublisher;

impl LoggingBundleEventPublisher {
    pub fn new() -> Self {
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

#[async_trait]
pub trait UserOpEventPublisher: Send + Sync {
    async fn publish(&self, event: UserOpEvent) -> Result<()>;

    async fn publish_all(&self, events: Vec<UserOpEvent>) -> Result<()>;
}

#[derive(Clone)]
pub struct KafkaUserOpEventPublisher {
    producer: FutureProducer,
    topic: String,
}

impl KafkaUserOpEventPublisher {
    pub fn new(producer: FutureProducer, topic: String) -> Self {
        Self { producer, topic }
    }

    async fn send_event(&self, event: &UserOpEvent) -> Result<()> {
        let user_op_hash = event.user_op_hash();
        let key = event.generate_event_key();
        let payload = serde_json::to_vec(event)?;

        let record = FutureRecord::to(&self.topic).key(&key).payload(&payload);

        match self
            .producer
            .send(record, tokio::time::Duration::from_secs(5))
            .await
        {
            Ok(_) => {
                debug!(
                    user_op_hash = %user_op_hash,
                    topic = %self.topic,
                    payload_size = payload.len(),
                    "Successfully published user op event"
                );
                Ok(())
            }
            Err((err, _)) => {
                error!(
                    user_op_hash = %user_op_hash,
                    topic = %self.topic,
                    error = %err,
                    "Failed to publish user op event"
                );
                Err(anyhow::anyhow!("Failed to publish user op event: {err}"))
            }
        }
    }
}

#[async_trait]
impl UserOpEventPublisher for KafkaUserOpEventPublisher {
    async fn publish(&self, event: UserOpEvent) -> Result<()> {
        self.send_event(&event).await
    }

    async fn publish_all(&self, events: Vec<UserOpEvent>) -> Result<()> {
        for event in events {
            self.send_event(&event).await?;
        }
        Ok(())
    }
}

#[derive(Clone)]
pub struct LoggingUserOpEventPublisher;

impl LoggingUserOpEventPublisher {
    pub fn new() -> Self {
        Self
    }
}

impl Default for LoggingUserOpEventPublisher {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl UserOpEventPublisher for LoggingUserOpEventPublisher {
    async fn publish(&self, event: UserOpEvent) -> Result<()> {
        info!(
            user_op_hash = %event.user_op_hash(),
            event = ?event,
            "Received user op event"
        );
        Ok(())
    }

    async fn publish_all(&self, events: Vec<UserOpEvent>) -> Result<()> {
        for event in events {
            self.publish(event).await?;
        }
        Ok(())
    }
}
