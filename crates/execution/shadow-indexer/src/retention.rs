//! Periodic retention sweeps that bound shadow block table growth.

use std::time::Duration;

use base_shadow_indexer_db::{ShadowDbConfig, ShadowRetentionRepo};
use reth_tasks::TaskExecutor;
use tokio::time::{MissedTickBehavior, interval};
use tracing::{debug, error, info, warn};

/// Sweeps run one statement at a time, so a single connection is enough.
const RETENTION_MAX_CONNECTIONS: u32 = 1;

/// Retention policy for persisted shadow blocks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShadowRetentionConfig {
    /// How long a row survives after it was last written.
    pub period: Duration,
    /// Delay between sweeps.
    pub interval: Duration,
}

/// Shadow block retention task.
#[derive(Debug)]
pub struct ShadowRetention {
    db_config: ShadowDbConfig,
    config: ShadowRetentionConfig,
}

impl ShadowRetention {
    /// Spawns the retention task on the provided executor.
    pub fn spawn(
        executor: &TaskExecutor,
        db_config: ShadowDbConfig,
        config: ShadowRetentionConfig,
    ) {
        let retention = Self {
            db_config: ShadowDbConfig { max_connections: RETENTION_MAX_CONNECTIONS, ..db_config },
            config,
        };

        // Unlike the writer this is not a critical task. Indexing stays correct when a sweep
        // fails, so a failure retries on the next tick instead of taking the builder down.
        executor.spawn_task(async move {
            retention.run().await;
        });
    }

    async fn run(self) {
        info!(
            target: "base::shadow-indexer",
            period = ?self.config.period,
            interval = ?self.config.interval,
            "Starting shadow block retention"
        );

        let mut ticker = interval(self.config.interval);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

        let repo = loop {
            ticker.tick().await;

            match self.db_config.init_pool().await {
                Ok(pool) => break ShadowRetentionRepo::new(pool),
                Err(error) => error!(
                    target: "base::shadow-indexer",
                    error = ?error,
                    "Failed to initialize the shadow retention database pool; \
                     retrying on the next tick"
                ),
            }
        };

        loop {
            if let Err(error) = Self::sweep_once(&repo, self.config.period).await {
                error!(
                    target: "base::shadow-indexer",
                    error = ?error,
                    "Shadow block retention sweep failed"
                );
            }

            ticker.tick().await;
        }
    }

    async fn sweep_once(repo: &ShadowRetentionRepo, period: Duration) -> anyhow::Result<()> {
        let cutoff = repo.cutoff(period).await?;

        let Some(sweep) = repo.sweep(cutoff).await? else {
            debug!(
                target: "base::shadow-indexer",
                "Another builder holds the shadow retention lock"
            );
            return Ok(());
        };

        if sweep.capped {
            warn!(
                target: "base::shadow-indexer",
                deleted = sweep.deleted,
                batches = sweep.batches,
                %cutoff,
                "Shadow block retention hit its per-sweep ceiling; backlog resumes next sweep"
            );
        } else if sweep.deleted > 0 {
            info!(
                target: "base::shadow-indexer",
                deleted = sweep.deleted,
                batches = sweep.batches,
                %cutoff,
                "Deleted expired shadow blocks"
            );
        }

        Ok(())
    }
}
