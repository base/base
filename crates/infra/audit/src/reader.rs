use crate::types::{BundleEvent, UserOpEvent};
use anyhow::Result;
use async_trait::async_trait;
use rdkafka::{
    Timestamp, TopicPartitionList,
    config::ClientConfig,
    consumer::{Consumer, StreamConsumer},
    message::Message,
};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tips_core::kafka::load_kafka_config_from_file;
use tokio::time::sleep;
use tracing::{debug, error};

pub fn create_kafka_consumer(kafka_properties_file: &str) -> Result<StreamConsumer> {
    let client_config: ClientConfig =
        ClientConfig::from_iter(load_kafka_config_from_file(kafka_properties_file)?);
    let consumer: StreamConsumer = client_config.create()?;
    Ok(consumer)
}

pub fn assign_topic_partition(consumer: &StreamConsumer, topic: &str) -> Result<()> {
    let mut tpl = TopicPartitionList::new();
    tpl.add_partition(topic, 0);
    consumer.assign(&tpl)?;
    Ok(())
}

#[derive(Debug, Clone)]
pub struct Event {
    pub key: String,
    pub event: BundleEvent,
    pub timestamp: i64,
}

#[async_trait]
pub trait EventReader {
    async fn read_event(&mut self) -> Result<Event>;
    async fn commit(&mut self) -> Result<()>;
}

pub struct KafkaAuditLogReader {
    consumer: StreamConsumer,
    topic: String,
    last_message_offset: Option<i64>,
    last_message_partition: Option<i32>,
}

impl KafkaAuditLogReader {
    pub fn new(consumer: StreamConsumer, topic: String) -> Result<Self> {
        consumer.subscribe(&[&topic])?;
        Ok(Self {
            consumer,
            topic,
            last_message_offset: None,
            last_message_partition: None,
        })
    }
}

#[async_trait]
impl EventReader for KafkaAuditLogReader {
    async fn read_event(&mut self) -> Result<Event> {
        match self.consumer.recv().await {
            Ok(message) => {
                let payload = message
                    .payload()
                    .ok_or_else(|| anyhow::anyhow!("Message has no payload"))?;

                // Extract Kafka timestamp, use current time as fallback
                let timestamp = match message.timestamp() {
                    Timestamp::CreateTime(millis) => millis,
                    Timestamp::LogAppendTime(millis) => millis,
                    Timestamp::NotAvailable => SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as i64,
                };

                let event: BundleEvent = serde_json::from_slice(payload)?;

                debug!(
                    bundle_id = %event.bundle_id(),
                    timestamp = timestamp,
                    offset = message.offset(),
                    partition = message.partition(),
                    "Received event with timestamp"
                );

                self.last_message_offset = Some(message.offset());
                self.last_message_partition = Some(message.partition());

                let key = message
                    .key()
                    .map(|k| String::from_utf8_lossy(k).to_string())
                    .ok_or_else(|| anyhow::anyhow!("Message missing required key"))?;

                let event_result = Event {
                    key,
                    event,
                    timestamp,
                };

                Ok(event_result)
            }
            Err(e) => {
                error!(error = %e, "Error receiving message from Kafka");
                sleep(Duration::from_secs(1)).await;
                Err(e.into())
            }
        }
    }

    async fn commit(&mut self) -> Result<()> {
        if let (Some(offset), Some(partition)) =
            (self.last_message_offset, self.last_message_partition)
        {
            let mut tpl = TopicPartitionList::new();
            tpl.add_partition_offset(&self.topic, partition, rdkafka::Offset::Offset(offset + 1))?;
            self.consumer
                .commit(&tpl, rdkafka::consumer::CommitMode::Async)?;
        }
        Ok(())
    }
}

impl KafkaAuditLogReader {
    pub fn topic(&self) -> &str {
        &self.topic
    }
}

#[derive(Debug, Clone)]
pub struct UserOpEventWrapper {
    pub key: String,
    pub event: UserOpEvent,
    pub timestamp: i64,
}

#[async_trait]
pub trait UserOpEventReader {
    async fn read_event(&mut self) -> Result<UserOpEventWrapper>;
    async fn commit(&mut self) -> Result<()>;
}

pub struct KafkaUserOpAuditLogReader {
    consumer: StreamConsumer,
    topic: String,
    last_message_offset: Option<i64>,
    last_message_partition: Option<i32>,
}

impl KafkaUserOpAuditLogReader {
    pub fn new(consumer: StreamConsumer, topic: String) -> Result<Self> {
        consumer.subscribe(&[&topic])?;
        Ok(Self {
            consumer,
            topic,
            last_message_offset: None,
            last_message_partition: None,
        })
    }
}

#[async_trait]
impl UserOpEventReader for KafkaUserOpAuditLogReader {
    async fn read_event(&mut self) -> Result<UserOpEventWrapper> {
        match self.consumer.recv().await {
            Ok(message) => {
                let payload = message
                    .payload()
                    .ok_or_else(|| anyhow::anyhow!("Message has no payload"))?;

                let timestamp = match message.timestamp() {
                    Timestamp::CreateTime(millis) => millis,
                    Timestamp::LogAppendTime(millis) => millis,
                    Timestamp::NotAvailable => SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as i64,
                };

                let event: UserOpEvent = serde_json::from_slice(payload)?;

                debug!(
                    user_op_hash = %event.user_op_hash(),
                    timestamp = timestamp,
                    offset = message.offset(),
                    partition = message.partition(),
                    "Received UserOp event"
                );

                self.last_message_offset = Some(message.offset());
                self.last_message_partition = Some(message.partition());

                let key = message
                    .key()
                    .map(|k| String::from_utf8_lossy(k).to_string())
                    .ok_or_else(|| anyhow::anyhow!("Message missing required key"))?;

                Ok(UserOpEventWrapper {
                    key,
                    event,
                    timestamp,
                })
            }
            Err(e) => {
                error!(error = %e, "Error receiving UserOp message from Kafka");
                sleep(Duration::from_secs(1)).await;
                Err(e.into())
            }
        }
    }

    async fn commit(&mut self) -> Result<()> {
        if let (Some(offset), Some(partition)) =
            (self.last_message_offset, self.last_message_partition)
        {
            let mut tpl = TopicPartitionList::new();
            tpl.add_partition_offset(&self.topic, partition, rdkafka::Offset::Offset(offset + 1))?;
            self.consumer
                .commit(&tpl, rdkafka::consumer::CommitMode::Async)?;
        }
        Ok(())
    }
}
