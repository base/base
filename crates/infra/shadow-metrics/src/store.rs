//! Postgres access for shadow metrics.

use anyhow::Result;
use sqlx::{PgPool, postgres::PgPoolOptions};

/// Schema readiness failure.
#[derive(Debug, thiserror::Error)]
pub enum ShadowMetricsSchemaReadinessError {
    /// Shadow blocks cannot be read.
    #[error(
        "shadow-metrics Postgres schema is not ready: one of the public.shadow_blocks columns number, hash, canonical_hash, created_at, updated_at, payload is not readable: {source}; verify database connectivity, apply the shadow-indexer migrations via ShadowDbConfig::init_pool, and grant the runtime role SELECT on public.shadow_blocks"
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
    /// Cursor columns cannot be read.
    #[error(
        "shadow-metrics Postgres schema is not ready: one of the public.shadow_metrics_cursor columns last_updated_at, last_number is not readable: {source}; apply the shadow-indexer migrations via ShadowDbConfig::init_pool and grant the runtime role SELECT on public.shadow_metrics_cursor"
    )]
    MetricsCursorNotReadable {
        /// Database error.
        #[source]
        source: sqlx::Error,
    },
    /// Columns this build cannot work against are still present.
    #[error(
        "shadow-metrics Postgres schema is not ready: columns {columns} are incompatible with this build; a listed public.shadow_blocks.reorged_out means the table still holds canonical rows that would be misreported as reorgs, and a listed public.shadow_metrics_cursor column is NOT NULL without a default so the cursor upsert can never fill it; apply the shadow-indexer migrations via ShadowDbConfig::init_pool, rolling shadow-indexer out before shadow-metrics"
    )]
    IncompatibleColumnsPresent {
        /// Offending `table.column` names.
        columns: String,
    },
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
    /// Columns `ShadowMetricsCursorRepo::store` binds on every cursor upsert.
    const CURSOR_UPSERT_COLUMNS: [&'static str; 4] =
        ["id", "last_updated_at", "last_number", "updated_at"];

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
        sqlx::query(
            "SELECT number, hash, canonical_hash, created_at, updated_at, payload \
             FROM public.shadow_blocks LIMIT 0",
        )
        .execute(&self.pool)
        .await
        .map_err(|source| ShadowMetricsSchemaReadinessError::ShadowBlocksNotReadable { source })?;

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
            None => return Err(ShadowMetricsSchemaReadinessError::MetricsCursorMissing),
            Some(false) => {
                return Err(ShadowMetricsSchemaReadinessError::MetricsCursorNotWritable);
            }
            Some(true) => {}
        }

        sqlx::query(
            "SELECT last_updated_at, last_number FROM public.shadow_metrics_cursor LIMIT 0",
        )
        .execute(&self.pool)
        .await
        .map_err(|source| ShadowMetricsSchemaReadinessError::MetricsCursorNotReadable { source })?;

        // The projections above cannot separate the pre- from the post-`0004` schema, because
        // every column this crate reads already existed before `0004`. Only columns `0004` drops
        // discriminate, and each is fatal in its own way: `shadow_blocks.reorged_out` means the
        // table still holds canonical rows the reader would misreport as reorgs, while any
        // `shadow_metrics_cursor` column that is NOT NULL, has no default, and is outside the set
        // the cursor upsert binds makes every cursor write fail.
        let incompatible_columns: Vec<String> = sqlx::query_scalar(
            "SELECT format('%s.%s', table_name, column_name) \
             FROM information_schema.columns \
             WHERE table_schema = 'public' \
               AND ( \
                   (table_name = 'shadow_blocks' AND column_name = 'reorged_out') \
                   OR ( \
                       table_name = 'shadow_metrics_cursor' \
                       AND is_nullable = 'NO' \
                       AND column_default IS NULL \
                       AND column_name <> ALL($1) \
                   ) \
               ) \
             ORDER BY 1",
        )
        .bind(&Self::CURSOR_UPSERT_COLUMNS[..])
        .fetch_all(&self.pool)
        .await
        .map_err(|source| ShadowMetricsSchemaReadinessError::QueryFailed { source })?;

        if !incompatible_columns.is_empty() {
            return Err(ShadowMetricsSchemaReadinessError::IncompatibleColumnsPresent {
                columns: incompatible_columns.join(", "),
            });
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn incompatible_columns_error_names_every_offending_column() {
        let error = ShadowMetricsSchemaReadinessError::IncompatibleColumnsPresent {
            columns: "shadow_blocks.reorged_out, shadow_metrics_cursor.legacy_not_null".to_owned(),
        }
        .to_string();

        assert!(error.starts_with("shadow-metrics Postgres schema is not ready:"), "{error}");
        assert!(error.contains("shadow_blocks.reorged_out"), "{error}");
        assert!(error.contains("shadow_metrics_cursor.legacy_not_null"), "{error}");
        assert!(error.contains("ShadowDbConfig::init_pool"), "{error}");
    }
}
