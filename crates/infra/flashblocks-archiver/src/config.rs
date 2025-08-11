use serde::{Deserialize, Serialize};
use url::Url;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    pub database: DatabaseConfig,
    pub builders: Vec<BuilderConfig>,
    pub archiver: ArchiverConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DatabaseConfig {
    pub url: String,
    pub max_connections: u32,
    pub connect_timeout_seconds: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BuilderConfig {
    pub name: String,
    pub url: Url,
    pub reconnect_delay_seconds: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ArchiverConfig {
    pub buffer_size: usize,
    pub batch_size: usize,
    pub flush_interval_seconds: u64,
}

impl Default for ArchiverConfig {
    fn default() -> Self {
        Self {
            buffer_size: 1000,
            batch_size: 100,
            flush_interval_seconds: 5,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            database: DatabaseConfig {
                url: "postgresql://localhost/flashblocks_archiver".to_string(),
                max_connections: 10,
                connect_timeout_seconds: 30,
            },
            builders: vec![],
            archiver: ArchiverConfig::default(),
        }
    }
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        let database_url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgresql://localhost/flashblocks_archiver".to_string());

        let max_connections = std::env::var("DATABASE_MAX_CONNECTIONS")
            .unwrap_or_else(|_| "10".to_string())
            .parse::<u32>()?;

        let builder_urls = std::env::var("FLASHBLOCKS_WEBSOCKET_URLS")
            .unwrap_or_default()
            .split(',')
            .filter(|s| !s.is_empty())
            .enumerate()
            .map(|(i, url)| {
                Ok(BuilderConfig {
                    name: format!("builder_{}", i),
                    url: Url::parse(url.trim())?,
                    reconnect_delay_seconds: 5,
                })
            })
            .collect::<anyhow::Result<Vec<_>>>()?;

        Ok(Self {
            database: DatabaseConfig {
                url: database_url,
                max_connections,
                connect_timeout_seconds: 30,
            },
            builders: builder_urls,
            archiver: ArchiverConfig::default(),
        })
    }
}
