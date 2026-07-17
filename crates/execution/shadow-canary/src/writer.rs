use std::time::Duration;

use chrono::Utc;
use reth_tasks::TaskExecutor;
use tokio::sync::mpsc;
use tokio::time::{MissedTickBehavior, interval};
use tracing::{error, info};

use base_shadow_canary_db::{ShadowBlockRepo, ShadowBlockRow, ShadowDbConfig};

const BATCH_SIZE: usize = 100;
const FLUSH_INTERVAL: Duration = Duration::from_secs(1);

/// Shadow canary writer task.
#[derive(Debug)]
pub struct ShadowWriter {
    rx: mpsc::Receiver<ShadowBlockRow>,
    db_config: ShadowDbConfig,
    builder_version: String,
}

impl ShadowWriter {
    /// Spawns the writer task on the provided executor.
    pub fn spawn(
        executor: TaskExecutor,
        rx: mpsc::Receiver<ShadowBlockRow>,
        db_config: ShadowDbConfig,
        builder_version: String,
    ) {
        let writer = Self { rx, db_config, builder_version };
        executor.spawn_critical_task("shadow-canary-writer", async move {
            writer.run().await;
        });
    }

    async fn run(mut self) {
        info!(target: "base::shadow-canary", "Starting shadow canary writer");

        let pool = match self.db_config.init_pool().await {
            Ok(pool) => pool,
            Err(error) => {
                error!(
                    target: "base::shadow-canary",
                    error = ?error,
                    "Failed to initialize shadow canary database pool"
                );
                return;
            }
        };
        let repo = ShadowBlockRepo::new(pool);
        let mut interval = interval(FLUSH_INTERVAL);
        interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut buffer = Vec::with_capacity(BATCH_SIZE);

        loop {
            tokio::select! {
                maybe_row = self.rx.recv() => {
                    match maybe_row {
                        Some(row) => {
                            buffer.push(row);
                            if buffer.len() >= BATCH_SIZE {
                                self.flush(&repo, &mut buffer).await;
                            }
                        }
                        None => {
                            if !buffer.is_empty() {
                                self.flush(&repo, &mut buffer).await;
                            }
                            break;
                        }
                    }
                }
                _ = interval.tick() => {
                    if !buffer.is_empty() {
                        self.flush(&repo, &mut buffer).await;
                    }
                }
            }
        }
    }

    async fn flush(&self, repo: &ShadowBlockRepo, buffer: &mut Vec<ShadowBlockRow>) {
        if buffer.is_empty() {
            return;
        }

        self.stamp_rows(buffer);
        let batch_size = buffer.len();

        match repo.insert_batch(buffer).await {
            Ok(inserted) => {
                info!(
                    target: "base::shadow-canary",
                    inserted,
                    batch_size,
                    "Inserted shadow canary rows"
                );
                buffer.clear();
            }
            Err(error) => {
                error!(
                    target: "base::shadow-canary",
                    error = ?error,
                    batch_size,
                    "Failed to insert shadow canary rows"
                );
            }
        }
    }

    fn stamp_rows(&self, rows: &mut [ShadowBlockRow]) {
        let created_at = Utc::now();
        for row in rows {
            row.builder_version = self.builder_version.clone();
            row.created_at = created_at;
        }
    }
}

/// Spawns the shadow canary writer task.
pub fn spawn_writer(
    executor: TaskExecutor,
    rx: mpsc::Receiver<ShadowBlockRow>,
    db_config: ShadowDbConfig,
    builder_version: String,
) {
    ShadowWriter::spawn(executor, rx, db_config, builder_version);
}
