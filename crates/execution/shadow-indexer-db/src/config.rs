use std::time::Duration;

use anyhow::{Context, Result};
use sqlx::{
    PgPool,
    postgres::{PgConnectOptions, PgPoolOptions},
};

/// Default Postgres port.
pub const DEFAULT_PORT: u16 = 5432;
/// Default shadow indexer database name.
pub const DEFAULT_DATABASE: &str = "shadow_metrics";
/// Default role owning the shadow indexer schema.
pub const DEFAULT_USERNAME: &str = "app";

/// Postgres connection parameters.
///
/// Discrete fields rather than a URL: the driver receives the password as a protocol
/// value, so a generated RDS password containing `@` or `/` needs no escaping and cannot
/// silently truncate a DSN.
#[derive(Clone, Debug)]
pub struct PgConnectionParams {
    /// Database host.
    pub host: String,
    /// Database port.
    pub port: u16,
    /// Database name.
    pub database: String,
    /// Role to authenticate as.
    pub username: String,
    /// Password for `username`.
    pub password: String,
}

impl Default for PgConnectionParams {
    fn default() -> Self {
        Self {
            host: String::new(),
            port: DEFAULT_PORT,
            database: DEFAULT_DATABASE.to_string(),
            username: DEFAULT_USERNAME.to_string(),
            password: String::new(),
        }
    }
}

impl PgConnectionParams {
    /// Build driver connect options.
    #[must_use]
    pub fn connect_options(&self) -> PgConnectOptions {
        PgConnectOptions::new()
            .host(&self.host)
            .port(self.port)
            .database(&self.database)
            .username(&self.username)
            .password(&self.password)
    }
}

/// Configuration for the shadow indexer database.
#[derive(Clone, Debug)]
pub struct ShadowDbConfig {
    /// Connection parameters.
    pub connection: PgConnectionParams,
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
            .connect_with(self.connection.connect_options())
            .await
            .context("failed to connect to shadow indexer database")?;

        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .context("failed to run shadow indexer database migrations")?;

        Ok(pool)
    }
}
