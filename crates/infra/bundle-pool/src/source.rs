use anyhow::Result;
use async_trait::async_trait;
use rdkafka::consumer::{Consumer, StreamConsumer};
use rdkafka::{ClientConfig, Message};
use std::fmt::Debug;
use tips_core::AcceptedBundle;
use tokio::sync::mpsc;
use tracing::{error, trace};

#[async_trait]
pub trait BundleSource {
    async fn run(&self) -> Result<()>;
}

pub struct KafkaBundleSource {
    queue_consumer: StreamConsumer,
    publisher: mpsc::UnboundedSender<AcceptedBundle>,
}

impl Debug for KafkaBundleSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "KafkaBundleSource")
    }
}

impl KafkaBundleSource {
    pub fn new(
        client_config: ClientConfig,
        topic: String,
        publisher: mpsc::UnboundedSender<AcceptedBundle>,
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

                    let accepted_bundle: AcceptedBundle = match serde_json::from_slice(payload) {
                        Ok(b) => b,
                        Err(e) => {
                            error!(error = %e, "Failed to deserialize bundle");
                            continue;
                        }
                    };

                    trace!(
                        bundle = ?accepted_bundle,
                        offset = message.offset(),
                        partition = message.partition(),
                        "Received bundle from Kafka"
                    );

                    if let Err(e) = self.publisher.send(accepted_bundle) {
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
