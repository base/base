use crate::domain::events::MempoolEvent;
use crate::services::interfaces::event_source::EventSource;
use async_trait::async_trait;
use rdkafka::{Message, consumer::StreamConsumer};
use serde_json;
use std::sync::Arc;

pub struct KafkaEventSource {
    consumer: Arc<StreamConsumer>,
}

impl KafkaEventSource {
    pub fn new(consumer: Arc<StreamConsumer>) -> Self {
        Self { consumer }
    }
}

#[async_trait]
impl EventSource for KafkaEventSource {
    async fn receive(&self) -> anyhow::Result<MempoolEvent> {
        let msg = self.consumer.recv().await?.detach();
        let payload = msg
            .payload()
            .ok_or_else(|| anyhow::anyhow!("Kafka message missing payload"))?;
        let event: MempoolEvent = serde_json::from_slice(payload)
            .map_err(|e| anyhow::anyhow!("Failed to parse Mempool event: {e}"))?;
        Ok(event)
    }
}
