use anyhow::Result;
use aws_config::{BehaviorVersion, Region};
use aws_credential_types::Credentials;
use aws_sdk_s3::{config::Builder as S3ConfigBuilder, Client as S3Client};
use clap::{Parser, ValueEnum};
use rdkafka::consumer::Consumer;
use tips_audit::{
    create_kafka_consumer, KafkaMempoolArchiver, KafkaMempoolReader, S3MempoolEventReaderWriter,
};
use tracing::{info, warn};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[derive(Debug, Clone, ValueEnum)]
enum S3ConfigType {
    Aws,
    Manual,
}

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[arg(long, env = "TIPS_AUDIT_KAFKA_BROKERS")]
    kafka_brokers: String,

    #[arg(long, env = "TIPS_AUDIT_KAFKA_TOPIC")]
    kafka_topic: String,

    #[arg(long, env = "TIPS_AUDIT_KAFKA_GROUP_ID")]
    kafka_group_id: String,

    #[arg(long, env = "TIPS_AUDIT_S3_BUCKET")]
    s3_bucket: String,

    #[arg(long, env = "TIPS_AUDIT_LOG_LEVEL", default_value = "info")]
    log_level: String,

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
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();

    let args = Args::parse();

    let log_level = match args.log_level.to_lowercase().as_str() {
        "trace" => tracing::Level::TRACE,
        "debug" => tracing::Level::DEBUG,
        "info" => tracing::Level::INFO,
        "warn" => tracing::Level::WARN,
        "error" => tracing::Level::ERROR,
        _ => {
            warn!(
                "Invalid log level '{}', defaulting to 'info'",
                args.log_level
            );
            tracing::Level::INFO
        }
    };

    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(log_level.to_string())),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    info!(
        kafka_brokers = %args.kafka_brokers,
        kafka_topic = %args.kafka_topic,
        kafka_group_id = %args.kafka_group_id,
        s3_bucket = %args.s3_bucket,
        "Starting audit archiver"
    );

    let consumer = create_kafka_consumer(&args.kafka_brokers, &args.kafka_group_id)?;
    consumer.subscribe(&[&args.kafka_topic])?;

    let reader = KafkaMempoolReader::new(consumer, args.kafka_topic.clone())?;

    let s3_client = create_s3_client(&args).await?;
    let s3_bucket = args.s3_bucket.clone();
    let writer = S3MempoolEventReaderWriter::new(s3_client, s3_bucket);

    let mut archiver = KafkaMempoolArchiver::new(reader, writer);

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
