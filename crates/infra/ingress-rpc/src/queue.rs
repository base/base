use std::sync::Arc;

use alloy_primitives::B256;
use anyhow::Result;
use async_trait::async_trait;
use backon::{ExponentialBuilder, Retryable};
use base_bundles::AcceptedBundle;
use rdkafka::producer::{FutureProducer, FutureRecord};
use tokio::time::Duration;
use tracing::{error, info};

/// Trait for publishing messages to a queue backend.
#[async_trait]
pub trait MessageQueue: Send + Sync {
    /// Publishes a message with the given key and payload to the specified topic.
    async fn publish(&self, topic: &str, key: &str, payload: &[u8]) -> Result<()>;
}

/// Kafka-backed message queue implementation.
pub struct KafkaMessageQueue {
    producer: FutureProducer,
}

impl std::fmt::Debug for KafkaMessageQueue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KafkaMessageQueue").finish_non_exhaustive()
    }
}

impl KafkaMessageQueue {
    /// Creates a new Kafka message queue with the given producer.
    pub const fn new(producer: FutureProducer) -> Self {
        Self { producer }
    }
}

#[async_trait]
impl MessageQueue for KafkaMessageQueue {
    async fn publish(&self, topic: &str, key: &str, payload: &[u8]) -> Result<()> {
        let enqueue = || async {
            let record = FutureRecord::to(topic).key(key).payload(payload);

            match self.producer.send(record, Duration::from_secs(5)).await {
                Ok((partition, offset)) => {
                    info!(
                        key = %key,
                        partition = partition,
                        offset = offset,
                        topic = %topic,
                        "Successfully enqueued message"
                    );
                    Ok(())
                }
                Err((err, _)) => {
                    error!(
                        key = key,
                        error = %err,
                        topic = topic,
                        "Failed to enqueue message"
                    );
                    Err(anyhow::anyhow!("Failed to enqueue bundle: {err}"))
                }
            }
        };

        enqueue
            .retry(
                &ExponentialBuilder::default()
                    .with_min_delay(Duration::from_millis(100))
                    .with_max_delay(Duration::from_secs(5))
                    .with_max_times(3),
            )
            .notify(|err: &anyhow::Error, dur: Duration| {
                info!("retrying to enqueue message {:?} after {:?}", err, dur);
            })
            .await
    }
}

/// Publishes accepted bundles to a message queue topic.
#[derive(Debug)]
pub struct BundleQueuePublisher<Q: MessageQueue> {
    queue: Arc<Q>,
    topic: String,
}

impl<Q: MessageQueue> BundleQueuePublisher<Q> {
    /// Creates a new publisher targeting the given queue and topic.
    pub const fn new(queue: Arc<Q>, topic: String) -> Self {
        Self { queue, topic }
    }

    /// Publishes the bundle with its hash as the message key.
    pub async fn publish(&self, bundle: &AcceptedBundle, hash: &B256) -> Result<()> {
        let key = hash.to_string();
        let payload = serde_json::to_vec(bundle)?;
        self.queue.publish(&self.topic, &key, &payload).await
    }
}

#[cfg(test)]
mod tests {
    use base_bundles::BundleExtensions;
    use base_bundles::{AcceptedBundle, Bundle};
    use base_bundles::test_utils::create_test_meter_bundle_response;
    use rdkafka::config::ClientConfig;
    use tokio::time::{Duration, Instant};

    use super::*;

    fn create_test_bundle() -> Bundle {
        Bundle::default()
    }

    #[tokio::test]
    async fn test_backoff_retry_logic() {
        // use an invalid broker address to trigger the backoff logic
        let producer = ClientConfig::new()
            .set("bootstrap.servers", "localhost:9999")
            .set("message.timeout.ms", "100")
            .create()
            .expect("Producer creation failed");

        let publisher = KafkaMessageQueue::new(producer);
        let bundle = create_test_bundle();
        let accepted_bundle =
            AcceptedBundle::new(bundle.try_into().unwrap(), create_test_meter_bundle_response());
        let bundle_hash = &accepted_bundle.bundle_hash();

        let start = Instant::now();
        let result = publisher
            .publish(
                "tips-ingress-rpc",
                bundle_hash.to_string().as_str(),
                &serde_json::to_vec(&accepted_bundle).unwrap(),
            )
            .await;
        let elapsed = start.elapsed();

        // the backoff tries at minimum 100ms, so verify we tried at least once
        assert!(result.is_err());
        assert!(elapsed >= Duration::from_millis(100));
    }
}
