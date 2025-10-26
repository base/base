use anyhow::Result;
use backon::{ExponentialBuilder, Retryable};
use clap::Parser;
use rdkafka::{
    config::ClientConfig,
    consumer::{Consumer, StreamConsumer},
    message::Message,
    producer::FutureProducer,
};
use std::fs;
use tips_audit::{BundleEvent, BundleEventPublisher, KafkaBundleEventPublisher};
use tips_common::BundleWithMetadata;
use tips_datastore::{BundleDatastore, postgres::PostgresDatastore};
use tokio::time::Duration;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[arg(long, env = "TIPS_INGRESS_WRITER_DATABASE_URL")]
    database_url: String,

    #[arg(long, env = "TIPS_INGRESS_WRITER_KAFKA_PROPERTIES_FILE")]
    kafka_properties_file: String,

    #[arg(long, env = "TIPS_INGRESS_KAFKA_TOPIC", default_value = "tips-ingress")]
    ingress_topic: String,

    #[arg(
        long,
        env = "TIPS_INGRESS_WRITER_AUDIT_TOPIC",
        default_value = "tips-audit"
    )]
    audit_topic: String,

    #[arg(long, env = "TIPS_INGRESS_WRITER_LOG_LEVEL", default_value = "info")]
    log_level: String,
}

/// IngressWriter consumes bundles sent from the Ingress service and writes them to the datastore
pub struct IngressWriter<Store, Publisher> {
    queue_consumer: StreamConsumer,
    datastore: Store,
    publisher: Publisher,
}

impl<Store, Publisher> IngressWriter<Store, Publisher>
where
    Store: BundleDatastore + Send + Sync + 'static,
    Publisher: BundleEventPublisher + Sync + Send + 'static,
{
    pub fn new(
        queue_consumer: StreamConsumer,
        queue_topic: String,
        datastore: Store,
        publisher: Publisher,
    ) -> Result<Self> {
        queue_consumer.subscribe(&[queue_topic.as_str()])?;
        Ok(Self {
            queue_consumer,
            datastore,
            publisher,
        })
    }

    async fn insert_bundle(&self) -> Result<(Uuid, BundleWithMetadata)> {
        match self.queue_consumer.recv().await {
            Ok(message) => {
                let payload = message
                    .payload()
                    .ok_or_else(|| anyhow::anyhow!("Message has no payload"))?;
                let bundle: BundleWithMetadata = serde_json::from_slice(payload)?;
                debug!(
                    bundle = ?bundle,
                    offset = message.offset(),
                    partition = message.partition(),
                    "Received bundle from queue"
                );

                let insert = || async {
                    self.datastore
                        .insert_bundle(bundle.clone())
                        .await
                        .map_err(|e| anyhow::anyhow!("Failed to insert bundle: {e}"))
                };

                let bundle_id = insert
                    .retry(
                        &ExponentialBuilder::default()
                            .with_min_delay(Duration::from_millis(100))
                            .with_max_delay(Duration::from_secs(5))
                            .with_max_times(3),
                    )
                    .notify(|err: &anyhow::Error, dur: Duration| {
                        info!("Retrying to insert bundle {:?} after {:?}", err, dur);
                    })
                    .await
                    .map_err(|e| anyhow::anyhow!("Failed to insert bundle after retries: {e}"))?;

                Ok((bundle_id, bundle))
            }
            Err(e) => {
                error!(error = %e, "Error receiving message from Kafka");
                Err(e.into())
            }
        }
    }

    async fn publish(&self, bundle_id: Uuid, bundle: &BundleWithMetadata) {
        if let Err(e) = self
            .publisher
            .publish(BundleEvent::Created {
                bundle_id,
                bundle: bundle.bundle.clone(),
            })
            .await
        {
            warn!(error = %e, bundle_id = %bundle_id, "Failed to publish BundleEvent::Created");
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    let args = Args::parse();

    tracing_subscriber::fmt()
        .with_env_filter(&args.log_level)
        .init();

    let config = load_kafka_config_from_file(&args.kafka_properties_file)?;
    let kafka_producer: FutureProducer = config.create()?;

    let publisher = KafkaBundleEventPublisher::new(kafka_producer, args.audit_topic.clone());
    let consumer = config.create()?;

    let bundle_store = PostgresDatastore::connect(args.database_url).await?;
    bundle_store.run_migrations().await?;

    let writer = IngressWriter::new(
        consumer,
        args.ingress_topic.clone(),
        bundle_store,
        publisher,
    )?;

    info!(
        "Ingress Writer service started, consuming from topic: {}",
        args.ingress_topic
    );
    loop {
        match writer.insert_bundle().await {
            Ok((bundle_id, bundle)) => {
                info!(bundle_id = %bundle_id, "Successfully inserted bundle");
                writer.publish(bundle_id, &bundle).await;
            }
            Err(e) => {
                error!(error = %e, "Failed to process bundle");
            }
        }
    }
}

fn load_kafka_config_from_file(properties_file_path: &str) -> Result<ClientConfig> {
    let kafka_properties = fs::read_to_string(properties_file_path)?;
    info!("Kafka properties:\n{}", kafka_properties);

    let mut client_config = ClientConfig::new();

    for line in kafka_properties.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            client_config.set(key.trim(), value.trim());
        }
    }

    Ok(client_config)
}
