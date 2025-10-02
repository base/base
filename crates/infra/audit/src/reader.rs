use crate::types::BundleEvent;
use anyhow::Result;
use async_trait::async_trait;
use rdkafka::{
    Timestamp, TopicPartitionList,
    config::ClientConfig,
    consumer::{Consumer, StreamConsumer},
    message::Message,
};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::time::sleep;
use tracing::{debug, error};

pub fn create_kafka_consumer(kafka_brokers: &str, group_id: &str) -> Result<StreamConsumer> {
    let consumer: StreamConsumer = ClientConfig::new()
        .set("group.id", group_id)
        .set("bootstrap.servers", kafka_brokers)
        .set("enable.partition.eof", "false")
        .set("session.timeout.ms", "6000")
        .set("enable.auto.commit", "false")
        .set("auto.offset.reset", "earliest")
        .set("fetch.wait.max.ms", "100")
        .set("fetch.min.bytes", "1")
        .create()?;
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

pub struct KafkaMempoolReader {
    consumer: StreamConsumer,
    topic: String,
    last_message_offset: Option<i64>,
    last_message_partition: Option<i32>,
}

impl KafkaMempoolReader {
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
impl EventReader for KafkaMempoolReader {
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
                println!("received error {e:?}");
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

impl KafkaMempoolReader {
    pub fn topic(&self) -> &str {
        &self.topic
    }
}
