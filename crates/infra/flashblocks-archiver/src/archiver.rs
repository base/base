use crate::database::Database;
use crate::websocket::WebSocketPool;
use crate::{config::Config, FlashblockMessage};
use anyhow::Result;
use std::collections::HashMap;
use std::time::Duration;
use tokio::time::{interval, timeout};
use tracing::{error, info, warn};
use uuid::Uuid;

#[derive(Debug)]
pub struct FlashblocksArchiver {
    config: Config,
    database: Database,
    builder_ids: HashMap<String, Uuid>,
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

        let mut stored_count = 0;
        let mut error_count = 0;

        for (builder_id, payload) in batch.drain(..) {
            match timeout(
                Duration::from_secs(30),
                self.database.store_flashblock(builder_id, &payload),
            )
            .await
            {
                Ok(Ok(_)) => {
                    stored_count += 1;
                    info!(
                        "Stored flashblock: block {}, index {}, payload_id {}",
                        payload.metadata.block_number, payload.index, payload.payload_id
                    );
                }
                Ok(Err(e)) => {
                    error_count += 1;
                    error!(
                        "Failed to store flashblock (block {}, index {}): {}",
                        payload.metadata.block_number, payload.index, e
                    );
                }
                Err(_) => {
                    error_count += 1;
                    error!(
                        "Timeout storing flashblock (block {}, index {})",
                        payload.metadata.block_number, payload.index
                    );
                }
            }
        }

        if stored_count > 0 {
            info!("Successfully stored {} flashblock messages", stored_count);
        }
        if error_count > 0 {
            warn!("Failed to store {} flashblock messages", error_count);
        }

        Ok(())
    }

    pub async fn get_latest_block_numbers(&self) -> Result<HashMap<String, u64>> {
        let mut result = HashMap::new();

        for (builder_name, builder_id) in &self.builder_ids {
            if let Some(block_number) = self.database.get_latest_block_number(*builder_id).await? {
                result.insert(builder_name.clone(), block_number);
            }
        }

        Ok(result)
    }
}
