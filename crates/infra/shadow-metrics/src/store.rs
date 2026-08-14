//! Postgres access for reading shadow blocks and persisting the metrics cursor.

use anyhow::Result;
use sqlx::{PgPool, postgres::PgPoolOptions};

/// Error returned when the shadow-metrics Postgres schema is not ready.
#[derive(Debug, thiserror::Error)]
pub enum ShadowMetricsSchemaReadinessError {
    /// The shadow blocks table or its update timestamp cannot be read.
    #[error(
        "shadow-metrics Postgres schema is not ready: public.shadow_blocks.updated_at is not readable: {source}; verify database connectivity, apply the shadow-indexer migrations via ShadowDbConfig::init_pool, and grant the runtime role SELECT on public.shadow_blocks"
    )]
    ShadowBlocksNotReadable {
        /// Underlying database error.
        #[source]
        source: sqlx::Error,
    },
    /// The shadow metrics cursor table is missing.
    #[error(
        "shadow-metrics Postgres schema is not ready: public.shadow_metrics_cursor is missing; apply the shadow-indexer migrations via ShadowDbConfig::init_pool"
    )]
    MetricsCursorMissing,
    /// The runtime role cannot insert or update the shadow metrics cursor.
    #[error(
        "shadow-metrics Postgres schema is not ready: runtime role needs INSERT and UPDATE on public.shadow_metrics_cursor; grant both privileges to the runtime role"
    )]
    MetricsCursorNotWritable,
    /// A readiness query failed before readiness could be determined.
    #[error(
        "shadow-metrics Postgres schema readiness query failed: {source}; verify database connectivity and that the runtime role can inspect public.shadow_metrics_cursor"
    )]
    QueryFailed {
        /// Underlying database error.
        #[source]
        source: sqlx::Error,
    },
}

/// Postgres store used by the shadow metrics reader.
#[derive(Debug, Clone)]
pub struct ShadowMetricsStore {
    pool: PgPool,
}

impl ShadowMetricsStore {
    /// Connects to Postgres without running migrations.
    ///
    /// Mirrors the audit archiver pool policy: eagerly opens the full configured
    /// pool and keeps physical connections open with no lifetime or idle expiry,
    /// so RDS IAM startup tokens stay valid for the life of the process.
    ///
    /// # Errors
    ///
    /// Returns an error if the Postgres pool cannot connect.
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

    /// Creates a store from an existing pool.
    #[must_use]
    pub const fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Returns the Postgres pool used by the store.
    #[must_use]
    pub const fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Checks whether shadow-metrics Postgres storage is ready for runtime use.
    ///
    /// # Errors
    ///
    /// Returns an error when required shadow-indexer relations or runtime-role
    /// privileges are unavailable, or when readiness cannot be determined.
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
