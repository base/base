use std::{fmt, time::Duration};

use anyhow::{Context, Result};
use sqlx::{PgPool, postgres::PgPoolOptions};
use tracing::info;

/// Database configuration.
#[derive(Clone)]
pub struct DatabaseConfig {
    /// Connection URL.
    pub url: String,
    /// Maximum number of connections in the pool.
    pub max_connections: u32,
    /// Timeout for acquiring a connection.
    pub connection_timeout: Duration,
}

impl fmt::Debug for DatabaseConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DatabaseConfig")
            .field("url", &mask_password(&self.url))
            .field("max_connections", &self.max_connections)
            .field("connection_timeout", &self.connection_timeout)
            .finish()
    }
}

impl DatabaseConfig {
    /// Create database configuration from environment variables.
    ///
    /// # Errors
    ///
    /// Returns an error if any required environment variable is missing.
    pub fn from_env() -> Result<Self> {
        let host = std::env::var("POSTGRES_HOST")
            .context("POSTGRES_HOST environment variable is required")?;
        let port = std::env::var("POSTGRES_PORT")
            .context("POSTGRES_PORT environment variable is required")?;
        let db =
            std::env::var("POSTGRES_DB").context("POSTGRES_DB environment variable is required")?;
        let user = std::env::var("POSTGRES_USER")
            .context("POSTGRES_USER environment variable is required")?;
        let password = std::env::var("POSTGRES_PASSWORD")
            .context("POSTGRES_PASSWORD environment variable is required")?;

        let sslmode = std::env::var("POSTGRES_SSLMODE").unwrap_or_else(|_| "require".to_string());
        let url = format!("postgres://{user}:{password}@{host}:{port}/{db}?sslmode={sslmode}");

        Ok(Self { url, max_connections: 10, connection_timeout: Duration::from_secs(30) })
    }

    /// Initialize database connection pool.
    ///
    /// # Errors
    ///
    /// Returns an error if the connection cannot be established.
    pub async fn init_pool(&self) -> Result<PgPool> {
        info!(url = %mask_password(&self.url), "connecting to database");

        let pool = PgPoolOptions::new()
            .max_connections(self.max_connections)
            .acquire_timeout(self.connection_timeout)
            .connect(&self.url)
            .await
            .context("failed to connect to database")?;

        info!("database connection pool initialized");
        Ok(pool)
    }
}

/// Mask password in database URL for logging.
fn mask_password(url: &str) -> String {
    if let Some(at_pos) = url.rfind('@')
        && let Some(colon_pos) = url[..at_pos].rfind(':')
    {
        let mut masked = url.to_string();
        masked.replace_range(colon_pos + 1..at_pos, "****");
        return masked;
    }
    url.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mask_password_standard_url() {
        let url = "postgres://user:secret@localhost:5432/db";
        let masked = mask_password(url);
        assert_eq!(masked, "postgres://user:****@localhost:5432/db");
    }

    #[test]
    fn mask_password_no_password() {
        let url = "postgres://localhost:5432/db";
        let masked = mask_password(url);
        assert_eq!(masked, url);
    }
}
