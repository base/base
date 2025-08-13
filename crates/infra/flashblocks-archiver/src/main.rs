use clap::Parser;
use flashblocks_archiver::{FlashblocksArchiver, FlashblocksArchiverArgs};
use tracing::{error, info};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "flashblocks_archiver=info".into()),
        )
        .init();

    let args = FlashblocksArchiverArgs::parse();
    let builders = args.parse_builders()?;

    info!(message = "Starting flashblocks-archiver");
    info!(message = "Configured builders", count = builders.len());
    for builder in &builders {
        info!(message = "Builder configured", name = %builder.name, url = %builder.url);
    }

    let archiver = FlashblocksArchiver::new(args).await?;

    let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel();

    tokio::spawn(async move {
        tokio::signal::ctrl_c()
            .await
            .expect("Failed to listen for ctrl-c");
        let _ = shutdown_tx.send(());
    });

    tokio::select! {
        result = archiver.run() => {
            match result {
                Ok(_) => info!(message = "Archiver finished normally"),
                Err(e) => error!(message = "Archiver failed", error = %e),
            }
        }
        _ = &mut shutdown_rx => {
            info!(message = "Shutdown signal received, stopping archiver");
        }
    }

    Ok(())
}
