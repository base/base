use std::{
    fmt,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use sqlx::{
    PgPool,
    postgres::{PgConnectOptions, PgPoolOptions},
};
use tokio::time::sleep;
use tracing::{error, info};

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

        // Migrations run at node startup, from a critical task: a failure here panics the
        // builder, and this schema has taken the mainnet builder down that way before. Both
        // outcomes are logged so an operator staring at a crashloop can tell a migration failure
        // from a connection failure without attaching to the database, and can see how long the
        // run took before it succeeded or gave up.
        let started = Instant::now();
        let mut migrator = sqlx::migrate!("./migrations");
        let migrations = migrator.iter().count();

        let result = async {
            let mut conn = pool.acquire().await?;
            // The lock belongs to this session. Never return it to the pool with the lock held,
            // including when startup fails or this future is cancelled.
            conn.close_on_drop();

            // Match SQLx 0.8's PostgreSQL migration lock, including its database-name checksum,
            // so we still serialize with other SQLx migrators. A blocking pg_advisory_lock query
            // retains a snapshot while waiting. CREATE INDEX CONCURRENTLY can wait for that
            // snapshot while holding the lock the waiter needs, deadlocking startup. Poll outside SQL.
            let database: String =
                sqlx::query_scalar("SELECT current_database()").fetch_one(&mut *conn).await?;
            let lock_id = 0x3d32ad9e_i64
                * i64::from(
                    crc::Crc::<u32>::new(&crc::CRC_32_ISO_HDLC).checksum(database.as_bytes()),
                );
            while !sqlx::query_scalar::<_, bool>("SELECT pg_try_advisory_lock($1)")
                .bind(lock_id)
                .fetch_one(&mut *conn)
                .await?
            {
                sleep(Duration::from_millis(100)).await;
            }

            migrator.set_locking(false);
            migrator.run_direct(&mut *conn).await
        }
        .await;

        match result {
            Ok(()) => {
                info!(
                    target: "base::shadow-indexer",
                    migrations,
                    elapsed = ?started.elapsed(),
                    "Applied shadow indexer database migrations"
                );
                Ok(pool)
            }
            Err(error) => {
                error!(
                    target: "base::shadow-indexer",
                    error = %error,
                    migrations,
                    elapsed = ?started.elapsed(),
                    "Failed to run shadow indexer database migrations"
                );
                Err(error).context("failed to run shadow indexer database migrations")
            }
        }
    }
}
