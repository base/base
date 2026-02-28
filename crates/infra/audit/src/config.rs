use anyhow::Result;
use aws_config::{BehaviorVersion, Region};
use aws_credential_types::Credentials;
use aws_sdk_s3::{Client as S3Client, config::Builder as S3ConfigBuilder};
use base_cli_utils::{LogConfig, MetricsConfig};
use tracing::info;

/// S3 client configuration.
#[derive(Debug, Clone)]
pub enum S3Config {
    /// Use the default AWS SDK credential chain.
    Aws,
    /// Use manually specified credentials and endpoint.
    Manual {
        /// The S3-compatible endpoint URL.
        endpoint: String,
        /// The AWS region.
        region: String,
        /// The access key ID.
        access_key_id: String,
        /// The secret access key.
        secret_access_key: String,
    },
}

/// Configuration for the audit archiver service.
#[derive(Debug, Clone)]
pub struct AuditArchiverConfig {
    /// Path to the Kafka properties file.
    pub kafka_properties_file: String,
    /// Kafka topic to consume audit events from.
    pub kafka_topic: String,
    /// S3 bucket to archive events to.
    pub s3_bucket: String,
    /// S3 client configuration.
    pub s3: S3Config,
    /// Number of worker tasks for archiving.
    pub worker_pool_size: usize,
    /// Channel buffer size between reader and workers.
    pub channel_buffer_size: usize,
    /// Skip archiving (for clearing Kafka offsets only).
    pub noop_archive: bool,
    /// Logging configuration.
    pub log: LogConfig,
    /// Metrics configuration.
    pub metrics: MetricsConfig,
}

impl S3Config {
    /// Creates an S3 client from this configuration.
    pub async fn create_s3_client(&self) -> Result<S3Client> {
        match self {
            Self::Manual { endpoint, region, access_key_id, secret_access_key } => {
                let credentials =
                    Credentials::new(access_key_id, secret_access_key, None, None, "manual");
                let config = aws_config::defaults(BehaviorVersion::latest())
                    .region(Region::new(region.clone()))
                    .endpoint_url(endpoint)
                    .credentials_provider(credentials)
                    .load()
                    .await;
                let s3_config_builder = S3ConfigBuilder::from(&config).force_path_style(true);

                info!("manually configuring s3 client");
                Ok(S3Client::from_conf(s3_config_builder.build()))
            }
            Self::Aws => {
                info!("using aws s3 client");
                let config = aws_config::load_defaults(BehaviorVersion::latest()).await;
                Ok(S3Client::new(&config))
            }
        }
    }
}
