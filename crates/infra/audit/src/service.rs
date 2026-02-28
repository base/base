use anyhow::Result;
use rdkafka::consumer::Consumer;
use tracing::info;

use crate::{
    KafkaAuditArchiver, KafkaAuditLogReader, S3EventReaderWriter,
    config::AuditArchiverConfig,
    reader::create_kafka_consumer,
};

/// Runs the audit archiver service with the given configuration.
pub async fn run(config: AuditArchiverConfig) -> Result<()> {
    config.log.init_tracing_subscriber().expect("Failed to initialize tracing");

    config.metrics.init().expect("Failed to install Prometheus exporter");

    info!(
        kafka_properties_file = %config.kafka_properties_file,
        kafka_topic = %config.kafka_topic,
        s3_bucket = %config.s3_bucket,
        "starting audit archiver"
    );

    let consumer = create_kafka_consumer(&config.kafka_properties_file)?;
    consumer.subscribe(&[&config.kafka_topic])?;

    let reader = KafkaAuditLogReader::new(consumer, config.kafka_topic.clone())?;

    let s3_client = config.s3.create_s3_client().await?;
    let writer = S3EventReaderWriter::new(s3_client, config.s3_bucket.clone());

    let mut archiver = KafkaAuditArchiver::new(
        reader,
        writer,
        config.worker_pool_size,
        config.channel_buffer_size,
        config.noop_archive,
    );

    info!("audit archiver initialized, starting main loop");

    archiver.run().await
}
