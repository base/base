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
use tracing::{debug, error, info};

/// Creates a Kafka consumer from a properties file.
pub fn create_kafka_consumer(kafka_properties_file: &str) -> Result<StreamConsumer> {
    let client_config: ClientConfig =
        ClientConfig::from_iter(load_kafka_config_from_file(kafka_properties_file)?);
    let consumer: StreamConsumer = client_config.create()?;
    Ok(consumer)
}

/// Assigns a topic partition to a consumer.
pub fn assign_topic_partition(consumer: &StreamConsumer, topic: &str) -> Result<()> {
    let mut tpl = TopicPartitionList::new();
    tpl.add_partition(topic, 0);
    consumer.assign(&tpl)?;
    Ok(())
}

/// A bundle event with metadata from Kafka.
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

/// Reads bundle audit events from Kafka.
pub struct KafkaAuditLogReader {
    consumer: StreamConsumer,
    topic: String,
    last_message_offset: Option<i64>,
    last_message_partition: Option<i32>,
}

impl std::fmt::Debug for KafkaAuditLogReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KafkaAuditLogReader")
            .field("topic", &self.topic)
            .field("last_message_offset", &self.last_message_offset)
            .field("last_message_partition", &self.last_message_partition)
            .finish_non_exhaustive()
    }
}

impl KafkaAuditLogReader {
    /// Creates a new Kafka audit log reader.
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

                info!(
                    bundle_id = %event.bundle_id(),
                    tx_ids = ?event.transaction_ids(),
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
    /// Returns the topic this reader is subscribed to.
    pub fn topic(&self) -> &str {
        &self.topic
    }
}

/// A user operation event with metadata from Kafka.
#[derive(Debug, Clone)]
pub struct UserOpEventWrapper {
    /// The event key.
    pub key: String,
    /// The user operation event.
    pub event: UserOpEvent,
    /// The event timestamp in milliseconds.
    pub timestamp: i64,
}

/// Trait for reading user operation events.
#[async_trait]
pub trait UserOpEventReader {
    /// Reads the next user operation event.
    async fn read_event(&mut self) -> Result<UserOpEventWrapper>;
    /// Commits the last read message.
    async fn commit(&mut self) -> Result<()>;
}

/// Reads user operation audit events from Kafka.
pub struct KafkaUserOpAuditLogReader {
    consumer: StreamConsumer,
    topic: String,
    last_message_offset: Option<i64>,
    last_message_partition: Option<i32>,
}

impl std::fmt::Debug for KafkaUserOpAuditLogReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KafkaUserOpAuditLogReader")
            .field("topic", &self.topic)
            .field("last_message_offset", &self.last_message_offset)
            .field("last_message_partition", &self.last_message_partition)
            .finish_non_exhaustive()
    }
}

impl KafkaUserOpAuditLogReader {
    /// Creates a new Kafka user operation audit log reader.
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
