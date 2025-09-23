use crate::types::MempoolEvent;
use anyhow::Result;
use async_trait::async_trait;
use rdkafka::producer::{FutureProducer, FutureRecord};
use serde_json;
use tracing::{debug, error};
use uuid::Uuid;

#[async_trait]
pub trait MempoolEventPublisher: Send + Sync {
    async fn publish(&self, event: MempoolEvent) -> Result<()>;
}

#[derive(Clone)]
pub struct KafkaMempoolEventPublisher {
    producer: FutureProducer,
    topic: String,
}

impl KafkaMempoolEventPublisher {
    pub fn new(producer: FutureProducer, topic: String) -> Self {
        Self { producer, topic }
    }

    async fn send_event(&self, event: &MempoolEvent) -> Result<()> {
        let bundle_id = event.bundle_id();
        let key = format!("{}-{}", bundle_id, Uuid::new_v4());
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
                    "Successfully published event"
                );
                Ok(())
            }
            Err((err, _)) => {
                error!(
                    bundle_id = %bundle_id,
                    topic = %self.topic,
                    error = %err,
                    "Failed to publish event"
                );
                Err(anyhow::anyhow!("Failed to publish event: {err}"))
            }
        }
    }
}

#[async_trait]
impl MempoolEventPublisher for KafkaMempoolEventPublisher {
    async fn publish(&self, event: MempoolEvent) -> Result<()> {
        self.send_event(&event).await
    }
}
