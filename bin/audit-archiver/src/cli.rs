use audit_archiver_lib::{AuditArchiverConfig, S3Config};
use clap::{Parser, ValueEnum};

use crate::{LogArgs, MetricsArgs};

/// S3 configuration type selector.
#[derive(Debug, Clone, ValueEnum)]
pub(crate) enum S3ConfigType {
    /// Use the default AWS SDK credential chain.
    Aws,
    /// Use manually specified credentials and endpoint.
    Manual,
}

/// CLI entry point for the audit archiver service.
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
pub(crate) struct Args {
    /// Path to the Kafka properties file.
    #[arg(long, env = "TIPS_AUDIT_KAFKA_PROPERTIES_FILE")]
    pub kafka_properties_file: String,

    /// Kafka topic to consume audit events from.
    #[arg(long, env = "TIPS_AUDIT_KAFKA_TOPIC")]
    pub kafka_topic: String,

    /// S3 bucket to archive events to.
    #[arg(long, env = "TIPS_AUDIT_S3_BUCKET")]
    pub s3_bucket: String,

    /// Logging configuration.
    #[command(flatten)]
    pub log: LogArgs,

    /// Metrics configuration.
    #[command(flatten)]
    pub metrics: MetricsArgs,

    /// S3 configuration type.
    #[arg(long, env = "TIPS_AUDIT_S3_CONFIG_TYPE", default_value = "aws")]
    pub s3_config_type: S3ConfigType,

    /// S3-compatible endpoint URL (required for manual config type).
    #[arg(long, env = "TIPS_AUDIT_S3_ENDPOINT")]
    pub s3_endpoint: Option<String>,

    /// AWS region.
    #[arg(long, env = "TIPS_AUDIT_S3_REGION", default_value = "us-east-1")]
    pub s3_region: String,

    /// S3 access key ID (required for manual config type).
    #[arg(long, env = "TIPS_AUDIT_S3_ACCESS_KEY_ID")]
    pub s3_access_key_id: Option<String>,

    /// S3 secret access key (required for manual config type).
    #[arg(long, env = "TIPS_AUDIT_S3_SECRET_ACCESS_KEY")]
    pub s3_secret_access_key: Option<String>,

    /// Number of worker tasks for archiving.
    #[arg(long, env = "TIPS_AUDIT_WORKER_POOL_SIZE", default_value = "80")]
    pub worker_pool_size: usize,

    /// Channel buffer size between reader and workers.
    #[arg(long, env = "TIPS_AUDIT_CHANNEL_BUFFER_SIZE", default_value = "500")]
    pub channel_buffer_size: usize,

    /// Skip archiving (for clearing Kafka offsets only).
    #[arg(long, env = "TIPS_AUDIT_NOOP_ARCHIVE", default_value = "false")]
    pub noop_archive: bool,
}

impl TryFrom<Args> for AuditArchiverConfig {
    type Error = anyhow::Error;

    fn try_from(args: Args) -> Result<Self, Self::Error> {
        let s3 = match args.s3_config_type {
            S3ConfigType::Aws => S3Config::Aws,
            S3ConfigType::Manual => {
                let endpoint = args
                    .s3_endpoint
                    .ok_or_else(|| anyhow::anyhow!("s3_endpoint is required for manual S3 config"))?;
                let access_key_id = args.s3_access_key_id.ok_or_else(|| {
                    anyhow::anyhow!("s3_access_key_id is required for manual S3 config")
                })?;
                let secret_access_key = args.s3_secret_access_key.ok_or_else(|| {
                    anyhow::anyhow!("s3_secret_access_key is required for manual S3 config")
                })?;
                S3Config::Manual {
                    endpoint,
                    region: args.s3_region,
                    access_key_id,
                    secret_access_key,
                }
            }
        };

        Ok(Self {
            kafka_properties_file: args.kafka_properties_file,
            kafka_topic: args.kafka_topic,
            s3_bucket: args.s3_bucket,
            s3,
            worker_pool_size: args.worker_pool_size,
            channel_buffer_size: args.channel_buffer_size,
            noop_archive: args.noop_archive,
            log: args.log.into(),
            metrics: args.metrics.into(),
        })
    }
}
