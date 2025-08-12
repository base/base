use crate::database::Database;
use crate::metrics::Metrics;
use crate::websocket::WebSocketPool;
use crate::{config::Config, FlashblockMessage};
use anyhow::Result;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tokio::time::interval;
use tracing::{error, info, warn};
use uuid::Uuid;

#[derive(Debug)]
pub struct FlashblocksArchiver {
    config: Config,
    database: Database,
    builder_ids: HashMap<String, Uuid>,
    metrics: Metrics,
}

impl FlashblocksArchiver {
    pub async fn new(config: Config) -> Result<Self> {
        let database = Database::new(&config.database.url, config.database.max_connections).await?;

        // Run database migrations
        database.run_migrations().await?;

        // Register builders and get their IDs
        let mut builder_ids = HashMap::new();
        for builder_config in &config.builders {
            let builder_id = database
                .get_or_create_builder(builder_config.url.as_ref(), Some(&builder_config.name))
                .await?;
            builder_ids.insert(builder_config.name.clone(), builder_id);
        }

        Ok(Self {
            config,
            database,
            builder_ids,
            metrics: Metrics::default(),
        })
    }

    pub async fn run(&self) -> Result<()> {
        info!(
            "Starting FlashblocksArchiver with {} builders",
            self.config.builders.len()
        );

        if self.config.builders.is_empty() {
            warn!("No builders configured, archiver will not collect any data");
            return Ok(());
        }

        // Start WebSocket connections
        let ws_pool = WebSocketPool::new(self.config.builders.clone());
        let mut receiver = ws_pool.start().await?;

        // Set up batching
        let mut batch = Vec::with_capacity(self.config.archiver.batch_size);
        let mut flush_interval = interval(Duration::from_secs(
            self.config.archiver.flush_interval_seconds,
        ));

        info!("FlashblocksArchiver started, listening for flashblock messages");

        loop {
            tokio::select! {
                // Receive flashblock messages
                message = receiver.recv() => {
                    match message {
                        Some((builder_name, payload)) => {
                            if let Some(builder_id) = self.builder_ids.get(&builder_name) {
                                batch.push((*builder_id, payload));

                                // Flush if batch is full
                                if batch.len() >= self.config.archiver.batch_size {
                                    if let Err(e) = self.flush_batch(&mut batch).await {
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
                                if let Err(e) = self.flush_batch(&mut batch).await {
                                    error!("Failed to flush final batch: {}", e);
                                }
                            }
                            break;
                        }
                    }
                }

                // Periodic flush
                _ = flush_interval.tick() => {
                    if !batch.is_empty() {
                        if let Err(e) = self.flush_batch(&mut batch).await {
                            error!("Failed to flush batch on timer: {}", e);
                        }
                    }
                }
            }
        }

        info!("FlashblocksArchiver stopped");
        Ok(())
    }

    async fn flush_batch(&self, batch: &mut Vec<(Uuid, FlashblockMessage)>) -> Result<()> {
        if batch.is_empty() {
            return Ok(());
        }

        info!("Flushing batch of {} flashblock messages", batch.len());

        for (builder_id, payload) in batch.drain(..) {
            let start = Instant::now();
            match self.database.store_flashblock(builder_id, &payload).await {
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
            self.metrics
                .store_flashblock_duration
                .record(start.elapsed());
        }

        Ok(())
    }
}
