//! Postgres access for shadow metrics.

use anyhow::Result;
use base_shadow_indexer_db::PgConnectionParams;
use sqlx::{PgPool, postgres::PgPoolOptions};

/// Schema readiness failure.
#[derive(Debug, thiserror::Error)]
pub enum ShadowMetricsSchemaReadinessError {
    /// Shadow blocks cannot be read.
    #[error(
        "shadow-metrics Postgres schema is not ready: public.shadow_blocks.updated_at is not readable: {source}; verify database connectivity, apply the shadow-indexer migrations via ShadowDbConfig::init_pool, and grant the runtime role SELECT on public.shadow_blocks"
    )]
    ShadowBlocksNotReadable {
        /// Database error.
        #[source]
        source: sqlx::Error,
    },
}

/// Shadow metrics Postgres store.
#[derive(Debug, Clone)]
pub struct ShadowMetricsStore {
    pool: PgPool,
}

impl ShadowMetricsStore {
    /// Connects without migrations, retaining eager connections for RDS IAM token lifetime.
    ///
    /// # Errors
    /// Returns an error when the pool cannot connect.
    pub async fn connect(connection: &PgConnectionParams, max_connections: u32) -> Result<Self> {
        let max_connections = max_connections.max(1);
        let pool = PgPoolOptions::new()
            .max_connections(max_connections)
            .min_connections(max_connections)
            .max_lifetime(None)
            .idle_timeout(None)
            .connect_with(connection.connect_options())
            .await?;
        Ok(Self { pool })
    }

    /// Wraps an existing pool.
    #[must_use]
    pub const fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Returns the pool.
    #[must_use]
    pub const fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Checks that shadow blocks are readable by the runtime role.
    ///
    /// # Errors
    /// Returns an error when the shadow block table cannot be read.
    pub async fn check_schema_ready(&self) -> Result<(), ShadowMetricsSchemaReadinessError> {
        sqlx::query("SELECT updated_at FROM public.shadow_blocks LIMIT 0")
            .execute(&self.pool)
            .await
            .map_err(|source| ShadowMetricsSchemaReadinessError::ShadowBlocksNotReadable {
                source,
            })?;

        Ok(())
    }
}
