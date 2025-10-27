use anyhow::Result;
use async_trait::async_trait;
use rdkafka::consumer::{Consumer, StreamConsumer};
use rdkafka::{ClientConfig, Message};
use tips_core::{Bundle, BundleWithMetadata};
use tokio::sync::mpsc;
use tracing::{debug, error};

#[async_trait]
pub trait BundleSource {
    async fn run(&self) -> Result<()>;
}

pub struct KafkaBundleSource {
    queue_consumer: StreamConsumer,
    publisher: mpsc::UnboundedSender<BundleWithMetadata>,
}

impl KafkaBundleSource {
    pub fn new(
        client_config: ClientConfig,
        topic: String,
        publisher: mpsc::UnboundedSender<BundleWithMetadata>,
    ) -> Result<Self> {
        let queue_consumer: StreamConsumer = client_config.create()?;
        queue_consumer.subscribe(&[topic.as_str()])?;
        Ok(Self {
            queue_consumer,
            publisher,
        })
    }
}

#[async_trait]
impl BundleSource for KafkaBundleSource {
    async fn run(&self) -> Result<()> {
        loop {
            match self.queue_consumer.recv().await {
                Ok(message) => {
                    let payload = match message.payload() {
                        Some(p) => p,
                        None => {
                            error!("Message has no payload");
                            continue;
                        }
                    };

                    let bundle: Bundle = match serde_json::from_slice(payload) {
                        Ok(b) => b,
                        Err(e) => {
                            error!(error = %e, "Failed to deserialize bundle");
                            continue;
                        }
                    };

                    debug!(
                        bundle = ?bundle,
                        offset = message.offset(),
                        partition = message.partition(),
                        "Received bundle from Kafka"
                    );

                    let bundle_with_metadata = match BundleWithMetadata::load(bundle) {
                        Ok(b) => b,
                        Err(e) => {
                            error!(error = %e, "Failed to load bundle");
                            continue;
                        }
                    };

                    if let Err(e) = self.publisher.send(bundle_with_metadata) {
                        error!(error = ?e, "Failed to publish bundle to queue");
                    }
                }
                Err(e) => {
                    error!(error = %e, "Error receiving message from Kafka");
                }
            }
        }
    }
}
