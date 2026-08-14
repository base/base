use std::time::Duration;

use anyhow::{Context, Result};
use sqlx::{PgPool, postgres::PgPoolOptions};

/// Configuration for the shadow indexer database.
#[derive(Clone, Debug)]
pub struct ShadowDbConfig {
    /// Database connection URL.
    pub url: String,
    /// Maximum number of open connections.
    pub max_connections: u32,
    /// Timeout when acquiring a connection.
    pub connection_timeout: Duration,
}

impl ShadowDbConfig {
    /// Initialize the database connection pool and run migrations.
    ///
    /// # Errors
    ///
    /// Returns an error when the connection or migrations fail.
    pub async fn init_pool(&self) -> Result<PgPool> {
        let pool = PgPoolOptions::new()
            .max_connections(self.max_connections)
            .acquire_timeout(self.connection_timeout)
            .connect(&self.url)
            .await
            .context("failed to connect to shadow indexer database")?;

        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .context("failed to run shadow indexer database migrations")?;

        Ok(pool)
    }
}
