use alloy_primitives::B256;
use alloy_rpc_types_mev::EthSendBundle;
use anyhow::Result;
use async_trait::async_trait;
use backon::{ExponentialBuilder, Retryable};
use rdkafka::producer::{FutureProducer, FutureRecord};
use tokio::time::Duration;
use tracing::{error, info};

/// A queue to buffer transactions
#[async_trait]
pub trait QueuePublisher: Send + Sync {
    async fn publish(&self, bundle: &EthSendBundle, bundle_hash: &B256) -> Result<()>;
}

/// A queue to buffer transactions
pub struct KafkaQueuePublisher {
    producer: FutureProducer,
    topic: String,
}

impl KafkaQueuePublisher {
    pub fn new(producer: FutureProducer, topic: String) -> Self {
        Self { producer, topic }
    }
}

#[async_trait]
impl QueuePublisher for KafkaQueuePublisher {
    async fn publish(&self, bundle: &EthSendBundle, bundle_hash: &B256) -> Result<()> {
        let key = bundle_hash.to_string();
        let payload = serde_json::to_vec(bundle)?;

        let enqueue = || async {
            let record = FutureRecord::to(&self.topic).key(&key).payload(&payload);

            match self.producer.send(record, Duration::from_secs(5)).await {
                Ok((partition, offset)) => {
                    info!(
                        bundle_hash = %bundle_hash,
                        partition = partition,
                        offset = offset,
                        topic = %self.topic,
                        "Successfully enqueued bundle"
                    );
                    Ok(())
                }
                Err((err, _)) => {
                    error!(
                        bundle_hash = %bundle_hash,
                        error = %err,
                        topic = %self.topic,
                        "Failed to enqueue bundle"
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
                info!("retrying to enqueue bundle {:?} after {:?}", err, dur);
            })
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rdkafka::config::ClientConfig;
    use tokio::time::{Duration, Instant};

    fn create_test_bundle() -> EthSendBundle {
        EthSendBundle::default()
    }

    #[tokio::test]
    async fn test_backoff_retry_logic() {
        // use an invalid broker address to trigger the backoff logic
        let producer = ClientConfig::new()
            .set("bootstrap.servers", "localhost:9999")
            .set("message.timeout.ms", "100")
            .create()
            .expect("Producer creation failed");

        let publisher = KafkaQueuePublisher::new(producer, "tips-ingress-rpc".to_string());
        let bundle = create_test_bundle();
        let bundle_hash = bundle.bundle_hash();

        let start = Instant::now();
        let result = publisher.publish(&bundle, &bundle_hash).await;
        let elapsed = start.elapsed();

        // the backoff tries at minimum 100ms, so verify we tried at least once
        assert!(result.is_err());
        assert!(elapsed >= Duration::from_millis(100));
    }
}
