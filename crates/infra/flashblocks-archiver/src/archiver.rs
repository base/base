use crate::database::Database;
use crate::metrics::Metrics;
use crate::websocket::WebSocketPool;
use crate::{cli::FlashblocksArchiverArgs, FlashblockMessage};
use anyhow::Result;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tokio::time::interval;
use tracing::{error, info, warn};
use uuid::Uuid;

#[derive(Debug)]
pub struct FlashblocksArchiver {
    args: FlashblocksArchiverArgs,
    database: Database,
    builder_ids: HashMap<String, Uuid>,
    metrics: Metrics,
}

impl FlashblocksArchiver {
    pub async fn new(args: FlashblocksArchiverArgs) -> Result<Self> {
        let database = Database::new(&args.database_url, args.database_max_connections).await?;

        database.run_migrations().await?;

        let builders = args.parse_builders()?;
        let mut builder_ids = HashMap::new();
        for builder_config in &builders {
            let builder_id = database
                .get_or_create_builder(builder_config.url.as_ref(), Some(&builder_config.name))
                .await?;
            builder_ids.insert(builder_config.name.clone(), builder_id);
        }

        Ok(Self {
            args,
            database,
            builder_ids,
            metrics: Metrics::default(),
        })
    }

    pub async fn run(&self) -> Result<()> {
        let builders = self.args.parse_builders()?;
        info!(
            "Starting FlashblocksArchiver with {} builders",
            builders.len()
        );

        if builders.is_empty() {
            warn!("No builders configured, archiver will not collect any data");
            return Ok(());
        }

        let ws_pool = WebSocketPool::new(builders);
        let mut receiver = ws_pool.start().await?;

        let mut batch = Vec::with_capacity(self.args.batch_size);
        let mut flush_interval = interval(Duration::from_secs(
            self.args.flush_interval_seconds,
        ));

        info!("FlashblocksArchiver started, listening for flashblock messages");

        loop {
            tokio::select! {
                message = receiver.recv() => {
                    match message {
                        Some((builder_name, payload)) => {
                            if let Some(builder_id) = self.builder_ids.get(&builder_name) {
                                batch.push((*builder_id, payload));

                                if batch.len() >= self.args.batch_size {
                                    if let Err(e) = self.flush_batch(&mut batch) {
                                        error!("Failed to flush batch: {}", e);
                                        self.metrics.flush_batch_error.increment(1);
                                    }
                                }
                            } else {
                                warn!("Received message from unknown builder: {}", builder_name);
                            }
                        }
                        None => {
                            info!("All WebSocket connections closed, flushing remaining data");
                            if !batch.is_empty() {
                                if let Err(e) = self.flush_batch(&mut batch) {
                                    error!("Failed to flush final batch: {}", e);
                                }
                            }
                            break;
                        }
                    }
                }

                // Periodically flush messages even if batch isn't full
                _ = flush_interval.tick() => {
                    if !batch.is_empty() {
                        if let Err(e) = self.flush_batch(&mut batch) {
                            error!("Failed to flush batch on timer: {}", e);
                        }
                    }
                }
            }
        }

        info!("FlashblocksArchiver stopped");
        Ok(())
    }

    fn flush_batch(&self, batch: &mut Vec<(Uuid, FlashblockMessage)>) -> Result<()> {
        if batch.is_empty() {
            return Ok(());
        }

        info!("Flushing batch of {} flashblock messages", batch.len());

        for (builder_id, payload) in batch.drain(..) {
            let database = self.database.get_pool().clone();
            let metrics = self.metrics.clone();
            
            tokio::spawn(async move {
                let start = Instant::now();
                let db = Database::from_pool(database);
                match db.store_flashblock(builder_id, &payload).await {
                    Ok(_) => {
                        info!(
                            "Stored flashblock: block {}, index {}, payload_id {}",
                            payload.metadata.block_number, payload.index, payload.payload_id
                        );
                    }
                    Err(e) => {
                        error!(
                            "Failed to store flashblock (block {}, index {}): {}",
                            payload.metadata.block_number, payload.index, e
                        );
                    }
                }
                metrics.store_flashblock_duration.record(start.elapsed());
            });
        }

        Ok(())
    }
}
