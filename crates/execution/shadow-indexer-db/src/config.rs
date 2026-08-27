use std::{borrow::Cow, fmt, time::Duration};

use anyhow::{Context, Result};
use sqlx::{
    PgPool,
    migrate::Migrator,
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
#[derive(Clone)]
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

/// Redacts the password so tracing a [`ShadowDbConfig`] cannot leak it into logs.
impl fmt::Debug for PgConnectionParams {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PgConnectionParams")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("database", &self.database)
            .field("username", &self.username)
            .field("password", &"[REDACTED]")
            .finish()
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

        // Migration 7 is already recorded with its original checksum in some environments, while
        // larger databases still need longer than its embedded five-minute timeout. Keep the
        // recorded checksum stable, but execute pending copies with a thirty-minute timeout.
        let mut migrations = sqlx::migrate!("./migrations").iter().cloned().collect::<Vec<_>>();
        for migration in &mut migrations {
            if migration.version == 7 {
                migration.sql = Cow::Owned(migration.sql.replace(
                    "SET LOCAL statement_timeout = '300s';",
                    "SET LOCAL statement_timeout = '1800s';",
                ));
            }
        }
        let migrator = Migrator { migrations: Cow::Owned(migrations), ..Migrator::DEFAULT };

        migrator.run(&pool).await.context("failed to run shadow indexer database migrations")?;

        Ok(pool)
    }
}
