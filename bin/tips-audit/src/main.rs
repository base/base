use std::net::SocketAddr;

use anyhow::Result;
use aws_config::{BehaviorVersion, Region};
use aws_credential_types::Credentials;
use aws_sdk_s3::{Client as S3Client, config::Builder as S3ConfigBuilder};
use clap::{Parser, ValueEnum};
use rdkafka::consumer::Consumer;
use tips_audit_lib::{
    KafkaAuditArchiver, KafkaAuditLogReader, S3EventReaderWriter, create_kafka_consumer,
};
use tips_core::{logger::init_logger_with_format, metrics::init_prometheus_exporter};
use tracing::info;

#[derive(Debug, Clone, ValueEnum)]
enum S3ConfigType {
    Aws,
    Manual,
}

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[arg(long, env = "TIPS_AUDIT_KAFKA_PROPERTIES_FILE")]
    kafka_properties_file: String,

    #[arg(long, env = "TIPS_AUDIT_KAFKA_TOPIC")]
    kafka_topic: String,

    #[arg(long, env = "TIPS_AUDIT_S3_BUCKET")]
    s3_bucket: String,

    #[arg(long, env = "TIPS_AUDIT_LOG_LEVEL", default_value = "info")]
    log_level: String,

    #[arg(long, env = "TIPS_AUDIT_LOG_FORMAT", default_value = "pretty")]
    log_format: tips_core::logger::LogFormat,

    #[arg(long, env = "TIPS_AUDIT_S3_CONFIG_TYPE", default_value = "aws")]
    s3_config_type: S3ConfigType,

    #[arg(long, env = "TIPS_AUDIT_S3_ENDPOINT")]
    s3_endpoint: Option<String>,

    #[arg(long, env = "TIPS_AUDIT_S3_REGION", default_value = "us-east-1")]
    s3_region: String,

    #[arg(long, env = "TIPS_AUDIT_S3_ACCESS_KEY_ID")]
    s3_access_key_id: Option<String>,

    #[arg(long, env = "TIPS_AUDIT_S3_SECRET_ACCESS_KEY")]
    s3_secret_access_key: Option<String>,

    #[arg(long, env = "TIPS_AUDIT_METRICS_ADDR", default_value = "0.0.0.0:9002")]
    metrics_addr: SocketAddr,

    #[arg(long, env = "TIPS_AUDIT_WORKER_POOL_SIZE", default_value = "80")]
    worker_pool_size: usize,

    #[arg(long, env = "TIPS_AUDIT_CHANNEL_BUFFER_SIZE", default_value = "500")]
    channel_buffer_size: usize,

    #[arg(long, env = "TIPS_AUDIT_NOOP_ARCHIVE", default_value = "false")]
    noop_archive: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();

    let args = Args::parse();

    init_logger_with_format(&args.log_level, args.log_format);

    init_prometheus_exporter(args.metrics_addr).expect("Failed to install Prometheus exporter");

    info!(
        kafka_properties_file = %args.kafka_properties_file,
        kafka_topic = %args.kafka_topic,
        s3_bucket = %args.s3_bucket,
        metrics_addr = %args.metrics_addr,
        "Starting audit archiver"
    );

    let consumer = create_kafka_consumer(&args.kafka_properties_file)?;
    consumer.subscribe(&[&args.kafka_topic])?;

    let reader = KafkaAuditLogReader::new(consumer, args.kafka_topic.clone())?;

    let s3_client = create_s3_client(&args).await?;
    let s3_bucket = args.s3_bucket.clone();
    let writer = S3EventReaderWriter::new(s3_client, s3_bucket);

    let mut archiver = KafkaAuditArchiver::new(
        reader,
        writer,
        args.worker_pool_size,
        args.channel_buffer_size,
        args.noop_archive,
    );

    info!("Audit archiver initialized, starting main loop");

    archiver.run().await
}

async fn create_s3_client(args: &Args) -> Result<S3Client> {
    match args.s3_config_type {
        S3ConfigType::Manual => {
            let region = args.s3_region.clone();
            let mut config_builder =
                aws_config::defaults(BehaviorVersion::latest()).region(Region::new(region));

            if let Some(endpoint) = &args.s3_endpoint {
                config_builder = config_builder.endpoint_url(endpoint);
            }

            if let (Some(access_key), Some(secret_key)) =
                (&args.s3_access_key_id, &args.s3_secret_access_key)
            {
                let credentials = Credentials::new(access_key, secret_key, None, None, "manual");
                config_builder = config_builder.credentials_provider(credentials);
            }

            let config = config_builder.load().await;
            let s3_config_builder = S3ConfigBuilder::from(&config).force_path_style(true);

            info!(message = "manually configuring s3 client");
            Ok(S3Client::from_conf(s3_config_builder.build()))
        }
        S3ConfigType::Aws => {
            info!(message = "using aws s3 client");
            let config = aws_config::load_defaults(BehaviorVersion::latest()).await;
            Ok(S3Client::new(&config))
        }
    }
}
