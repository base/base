mod job;

use crate::job::MaintenanceJob;

use alloy_provider::{ProviderBuilder, RootProvider};
use anyhow::Result;
use base_reth_flashblocks_rpc::subscription::FlashblocksSubscriber;
use clap::Parser;
use op_alloy_network::Optimism;
use rdkafka::ClientConfig;
use rdkafka::producer::FutureProducer;
use std::fs;
use std::sync::Arc;
use tips_audit::KafkaBundleEventPublisher;
use tips_datastore::PostgresDatastore;
use tracing::{info, warn};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
use url::Url;

#[derive(Parser, Clone)]
#[command(author, version, about, long_about = None)]
pub struct Args {
    #[arg(long, env = "TIPS_MAINTENANCE_KAFKA_PROPERTIES_FILE")]
    pub kafka_properties_file: String,

    #[arg(
        long,
        env = "TIPS_MAINTENANCE_KAFKA_TOPIC",
        default_value = "tips-audit"
    )]
    pub kafka_topic: String,

    #[arg(long, env = "TIPS_MAINTENANCE_DATABASE_URL")]
    pub database_url: String,

    #[arg(long, env = "TIPS_MAINTENANCE_RPC_URL")]
    pub rpc_url: Url,

    #[arg(
        long,
        env = "TIPS_MAINTENANCE_RPC_POLL_INTERVAL_MS",
        default_value = "250"
    )]
    pub rpc_poll_interval: u64,

    #[arg(long, env = "TIPS_MAINTENANCE_FLASHBLOCKS_WS")]
    pub flashblocks_ws: Url,

    #[arg(long, env = "TIPS_MAINTENANCE_LOG_LEVEL", default_value = "info")]
    pub log_level: String,

    #[arg(long, env = "TIPS_MAINTENANCE_FINALIZATION_DEPTH", default_value = "4")]
    pub finalization_depth: u64,

    #[arg(
        long,
        env = "TIPS_MAINTENANCE_UPDATE_INCLUDED_BY_BUILDER",
        default_value = "true"
    )]
    pub update_included_by_builder: bool,

    #[arg(long, env = "TIPS_MAINTENANCE_INTERVAL_MS", default_value = "2000")]
    pub maintenance_interval_ms: u64,
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

    info!("Starting maintenance service");

    let provider: RootProvider<Optimism> = ProviderBuilder::new()
        .disable_recommended_fillers()
        .network::<Optimism>()
        .connect_http(args.rpc_url.clone());

    let datastore = PostgresDatastore::connect(args.database_url.clone()).await?;

    let client_config = load_kafka_config_from_file(&args.kafka_properties_file)?;
    let kafka_producer: FutureProducer = client_config.create()?;

    let publisher = KafkaBundleEventPublisher::new(kafka_producer, args.kafka_topic.clone());

    let (fb_tx, fb_rx) = tokio::sync::mpsc::unbounded_channel();

    let job = Arc::new(MaintenanceJob::new(
        datastore,
        provider,
        publisher,
        args.clone(),
        fb_tx,
    ));

    let mut flashblocks_client =
        FlashblocksSubscriber::new(job.clone(), args.flashblocks_ws.clone());
    flashblocks_client.start();

    job.run(fb_rx).await?;

    Ok(())
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
