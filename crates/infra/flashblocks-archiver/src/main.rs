use clap::Parser;
use flashblocks_archiver::{Config, FlashblocksArchiver};
use tracing::{error, info};

#[derive(Parser)]
#[command(name = "flashblocks-archiver")]
#[command(about = "Archives flashblock messages from multiple builders to PostgreSQL")]
struct Cli {
    #[arg(long, help = "Path to configuration file")]
    config: Option<String>,

    #[arg(long, help = "Database URL (overrides config file)")]
    database_url: Option<String>,

    #[arg(long, help = "Comma-separated list of builder WebSocket URLs")]
    builder_urls: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "flashblocks_archiver=info".into()),
        )
        .init();

    let cli = Cli::parse();

    // Load configuration
    let mut config = if let Some(config_path) = cli.config {
        let config_content = tokio::fs::read_to_string(&config_path).await?;
        serde_json::from_str(&config_content)?
    } else {
        Config::from_env()?
    };

    // Override with CLI arguments if provided
    if let Some(database_url) = cli.database_url {
        config.database.url = database_url;
    }

    if let Some(builder_urls) = cli.builder_urls {
        use flashblocks_archiver::BuilderConfig;
        use url::Url;

        config.builders = builder_urls
            .split(',')
            .filter(|s| !s.is_empty())
            .enumerate()
            .map(|(i, url)| {
                Ok(BuilderConfig {
                    name: format!("builder_{}", i),
                    url: Url::parse(url.trim())?,
                    reconnect_delay_seconds: 5,
                })
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
    }

    info!("Starting flashblocks-archiver");
    info!("Database URL: {}", config.database.url);
    info!("Configured builders: {}", config.builders.len());
    for builder in &config.builders {
        info!("  - {}: {}", builder.name, builder.url);
    }

    // Create and start the archiver
    let archiver = FlashblocksArchiver::new(config).await?;

    // Set up graceful shutdown
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel();

    tokio::spawn(async move {
        tokio::signal::ctrl_c()
            .await
            .expect("Failed to listen for ctrl-c");
        let _ = shutdown_tx.send(());
    });

    // Run the archiver
    tokio::select! {
        result = archiver.run() => {
            match result {
                Ok(_) => info!("Archiver finished normally"),
                Err(e) => error!("Archiver failed: {}", e),
            }
        }
        _ = &mut shutdown_rx => {
            info!("Shutdown signal received, stopping archiver...");
        }
    }

    Ok(())
}
