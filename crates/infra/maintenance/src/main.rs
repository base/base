use alloy_provider::network::TransactionResponse;
use alloy_provider::network::primitives::BlockTransactions;
use alloy_provider::{Provider, ProviderBuilder, RootProvider};
use anyhow::Result;
use clap::Parser;
use op_alloy_network::Optimism;
use rdkafka::ClientConfig;
use rdkafka::producer::FutureProducer;
use std::time::Duration;
use tips_audit::{KafkaMempoolEventPublisher, MempoolEvent, MempoolEventPublisher};
use tips_datastore::{BundleDatastore, PostgresDatastore};
use tokio::time::sleep;
use tracing::{error, info, warn};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
use url::Url;

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[arg(long, env = "TIPS_MAINTENANCE_RPC_NODE")]
    node_url: Url,

    #[arg(long, env = "TIPS_MAINTENANCE_KAFKA_BROKERS")]
    kafka_brokers: String,

    #[arg(
        long,
        env = "TIPS_MAINTENANCE_KAFKA_TOPIC",
        default_value = "mempool-events"
    )]
    kafka_topic: String,

    #[arg(long, env = "TIPS_MAINTENANCE_DATABASE_URL")]
    database_url: String,

    #[arg(long, env = "TIPS_MAINTENANCE_POLL_INTERVAL_MS", default_value = "250")]
    poll_interval: u64,

    #[arg(long, env = "TIPS_MAINTENANCE_LOG_LEVEL", default_value = "info")]
    log_level: String,
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
        .connect_http(args.node_url);

    let datastore = PostgresDatastore::connect(args.database_url).await?;

    let kafka_producer: FutureProducer = ClientConfig::new()
        .set("bootstrap.servers", &args.kafka_brokers)
        .set("message.timeout.ms", "5000")
        .create()?;

    let publisher = KafkaMempoolEventPublisher::new(kafka_producer, args.kafka_topic);

    let mut last_processed_block: Option<u64> = None;

    loop {
        match process_new_blocks(&provider, &datastore, &publisher, &mut last_processed_block).await
        {
            Ok(_) => {}
            Err(e) => {
                error!(message = "Error processing blocks", error=%e);
            }
        }

        sleep(Duration::from_millis(args.poll_interval)).await;
    }
}

async fn process_new_blocks(
    provider: &impl Provider<Optimism>,
    datastore: &PostgresDatastore,
    publisher: &KafkaMempoolEventPublisher,
    last_processed_block: &mut Option<u64>,
) -> Result<()> {
    let latest_block_number = provider.get_block_number().await?;

    let start_block = last_processed_block
        .map(|n| n + 1)
        .unwrap_or(latest_block_number);

    if start_block > latest_block_number {
        return Ok(());
    }

    info!(message = "Processing blocks", from=%start_block, to=%latest_block_number);

    for block_number in start_block..=latest_block_number {
        match process_block(provider, datastore, publisher, block_number).await {
            Ok(_) => {
                info!(message = "Successfully processed block", block=%block_number);
                *last_processed_block = Some(block_number);
            }
            Err(e) => {
                return Err(e);
            }
        }
    }

    Ok(())
}

async fn process_block(
    provider: &impl Provider<Optimism>,
    datastore: &PostgresDatastore,
    publisher: &KafkaMempoolEventPublisher,
    block_number: u64,
) -> Result<()> {
    let block = provider
        .get_block_by_number(block_number.into())
        .full()
        .await?
        .ok_or_else(|| anyhow::anyhow!("Block {} not found", block_number))?;

    let block_hash = block.header.hash;

    let transactions = match &block.transactions {
        BlockTransactions::Full(txs) => txs,
        BlockTransactions::Hashes(_) => {
            return Err(anyhow::anyhow!(
                "Block transactions returned as hashes only, expected full transactions"
            ));
        }
        BlockTransactions::Uncle => {
            return Err(anyhow::anyhow!("Block contains uncle transactions"));
        }
    };

    for tx in transactions {
        let tx_hash = tx.tx_hash();
        info!(message = "Processing transaction", tx_hash=%tx_hash);

        match datastore.find_bundle_by_transaction_hash(tx_hash).await? {
            Some(bundle_id) => {
                info!(message = "Found bundle for transaction", bundle_id=%bundle_id, tx_hash=%tx_hash);

                let event = MempoolEvent::BlockIncluded {
                    bundle_id,
                    block_number,
                    block_hash,
                };

                publisher.publish(event).await?;
                datastore.remove_bundle(bundle_id).await?;

                info!(message = "Removed bundle for transaction", bundle_id=%bundle_id, tx_hash=%tx_hash);
            }
            None => {
                error!(message = "Transaction not part of tracked bundle", tx_hash=%tx_hash);
            }
        }
    }

    Ok(())
}
