//! Postgres connectivity for the shadow-metrics noop service.

use anyhow::Result;
use sqlx::{PgPool, migrate::Migrator, postgres::PgPoolOptions};

const REQUIRED_SHADOW_METRICS_MIGRATION_DESCRIPTION: &str = "shadow metrics";
static SHADOW_METRICS_MIGRATOR: Migrator = sqlx::migrate!("./migrations");

/// Error returned when the shadow-metrics Postgres schema is not ready.
#[derive(Debug, thiserror::Error)]
pub enum ShadowMetricsSchemaReadinessError {
    /// sqlx migration metadata table is missing.
    #[error(
        "shadow-metrics Postgres schema is not ready: _sqlx_migrations is missing; run `shadow-metrics migrate up`"
    )]
    MigrationTableMissing,
    /// The required sqlx migration has not been applied successfully.
    #[error(
        "shadow-metrics Postgres schema is not ready: required sqlx migration version {required_version} has not been applied successfully; run `shadow-metrics migrate up`"
    )]
    RequiredMigrationMissing {
        /// Required sqlx migration version.
        required_version: i64,
    },
    /// An expected table is missing or not visible to the runtime role.
    #[error(
        "shadow-metrics Postgres schema is not ready: public.shadow_blocks and public.shadow_block_transactions must be present and visible to the runtime role; run `shadow-metrics migrate up`"
    )]
    ShadowMetricsRelationMissing,
    /// A readiness query failed before readiness could be determined.
    #[error(
        "shadow-metrics Postgres schema readiness query failed: {source}; verify database connectivity and runtime role privileges"
    )]
    QueryFailed {
        /// Underlying database error.
        #[source]
        source: sqlx::Error,
    },
    /// The embedded sqlx migration metadata is internally inconsistent.
    #[error("shadow-metrics Postgres schema readiness metadata is invalid: {reason}")]
    MigrationMetadataInvalid {
        /// Static metadata error.
        reason: &'static str,
    },
}

/// Postgres handle for the shadow-metrics noop service.
#[derive(Debug, Clone)]
pub struct ShadowMetricsSink {
    pool: PgPool,
}

impl ShadowMetricsSink {
    /// Returns the required sqlx migration version for shadow-metrics storage.
    pub fn required_migration_version() -> Result<i64, &'static str> {
        let mut matching_migrations = SHADOW_METRICS_MIGRATOR.iter().filter(|migration| {
            migration.description.as_ref() == REQUIRED_SHADOW_METRICS_MIGRATION_DESCRIPTION
        });
        let migration = matching_migrations.next().ok_or(
            "shadow-metrics migration 001_shadow_metrics.sql must be embedded in the migrator",
        )?;
        if matching_migrations.next().is_some() {
            return Err("shadow-metrics migration description must be unique");
        }
        Ok(migration.version)
    }

    /// Connects to Postgres without running migrations.
    ///
    /// Mirrors the audit archiver pool policy: eagerly opens the full configured
    /// pool and keeps physical connections open with no lifetime or idle expiry,
    /// so RDS IAM startup tokens stay valid for the life of the process.
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

    /// Runs pending Postgres migrations.
    pub async fn migrate(database_url: &str) -> Result<()> {
        let pool = PgPoolOptions::new().max_connections(1).connect(database_url).await?;
        SHADOW_METRICS_MIGRATOR.run(&pool).await?;
        Ok(())
    }

    /// Creates a sink from an existing pool.
    pub const fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Checks whether shadow-metrics Postgres storage is ready for runtime use.
    pub async fn check_schema_ready(&self) -> Result<(), ShadowMetricsSchemaReadinessError> {
        let (migration_table_exists, shadow_blocks_exists, shadow_block_transactions_exists): (
            bool,
            bool,
            bool,
        ) = sqlx::query_as(
            "SELECT \
                    to_regclass('_sqlx_migrations') IS NOT NULL AS migration_table_exists, \
                    to_regclass('public.shadow_blocks') IS NOT NULL AS shadow_blocks_exists, \
                    to_regclass('public.shadow_block_transactions') IS NOT NULL AS shadow_block_transactions_exists",
        )
        .fetch_one(&self.pool)
        .await
        .map_err(|source| ShadowMetricsSchemaReadinessError::QueryFailed { source })?;

        if !migration_table_exists {
            return Err(ShadowMetricsSchemaReadinessError::MigrationTableMissing);
        }

        let required_version = Self::required_migration_version().map_err(|reason| {
            ShadowMetricsSchemaReadinessError::MigrationMetadataInvalid { reason }
        })?;
        let migration_applied: Option<bool> =
            sqlx::query_scalar("SELECT success FROM _sqlx_migrations WHERE version = $1")
                .bind(required_version)
                .fetch_optional(&self.pool)
                .await
                .map_err(|source| ShadowMetricsSchemaReadinessError::QueryFailed { source })?;
        if !matches!(migration_applied, Some(true)) {
            return Err(ShadowMetricsSchemaReadinessError::RequiredMigrationMissing {
                required_version,
            });
        }

        if !shadow_blocks_exists || !shadow_block_transactions_exists {
            return Err(ShadowMetricsSchemaReadinessError::ShadowMetricsRelationMissing);
        }

        sqlx::query("SELECT 1 FROM shadow_blocks LIMIT 0")
            .execute(&self.pool)
            .await
            .map_err(|source| ShadowMetricsSchemaReadinessError::QueryFailed { source })?;
        sqlx::query("SELECT 1 FROM shadow_block_transactions LIMIT 0")
            .execute(&self.pool)
            .await
            .map_err(|source| ShadowMetricsSchemaReadinessError::QueryFailed { source })?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn required_migration_version_is_resolvable() {
        let version = ShadowMetricsSink::required_migration_version().unwrap();
        assert!(version > 0);
    }
}
