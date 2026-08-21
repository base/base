//! Postgres access for shadow metrics.

use anyhow::Result;
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
    /// Cursor table is missing.
    #[error(
        "shadow-metrics Postgres schema is not ready: public.shadow_metrics_cursor is missing; apply the shadow-indexer migrations via ShadowDbConfig::init_pool"
    )]
    MetricsCursorMissing,
    /// Cursor table is not writable.
    #[error(
        "shadow-metrics Postgres schema is not ready: runtime role needs INSERT and UPDATE on public.shadow_metrics_cursor; grant both privileges to the runtime role"
    )]
    MetricsCursorNotWritable,
    /// Readiness query failed.
    #[error(
        "shadow-metrics Postgres schema readiness query failed: {source}; verify database connectivity and that the runtime role can inspect public.shadow_metrics_cursor"
    )]
    QueryFailed {
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
    pub async fn connect(database_url: &str, max_connections: u32) -> Result<Self> {
        let max_connections = max_connections.max(1);
        let pool = PgPoolOptions::new()
            .max_connections(max_connections)
            .min_connections(max_connections)
            .max_lifetime(None)
            .idle_timeout(None)
            .connect(database_url)
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

    /// Checks schema and runtime privileges.
    ///
    /// # Errors
    /// Returns an error when schema, privileges, or readiness queries fail.
    pub async fn check_schema_ready(&self) -> Result<(), ShadowMetricsSchemaReadinessError> {
        sqlx::query("SELECT updated_at FROM public.shadow_blocks LIMIT 0")
            .execute(&self.pool)
            .await
            .map_err(|source| ShadowMetricsSchemaReadinessError::ShadowBlocksNotReadable {
                source,
            })?;

        let metrics_cursor_writable: Option<bool> = sqlx::query_scalar(
            "SELECT CASE \
                 WHEN to_regclass('public.shadow_metrics_cursor') IS NULL THEN NULL \
                 ELSE has_table_privilege( \
                     current_user, 'public.shadow_metrics_cursor', 'INSERT' \
                 ) AND has_table_privilege( \
                     current_user, 'public.shadow_metrics_cursor', 'UPDATE' \
                 ) \
             END",
        )
        .fetch_one(&self.pool)
        .await
        .map_err(|source| ShadowMetricsSchemaReadinessError::QueryFailed { source })?;

        match metrics_cursor_writable {
            None => Err(ShadowMetricsSchemaReadinessError::MetricsCursorMissing),
            Some(false) => Err(ShadowMetricsSchemaReadinessError::MetricsCursorNotWritable),
            Some(true) => Ok(()),
        }
    }
}
