use anyhow::Result;
use clap::Parser;
use url::Url;

#[derive(Parser, Debug, Clone)]
#[command(name = "flashblocks-archiver")]
#[command(about = "Archives flashblock messages from multiple builders to PostgreSQL")]
pub struct FlashblocksArchiverArgs {
    #[arg(
        long,
        env = "DATABASE_URL",
        default_value = "postgresql://localhost/flashblocks_archiver",
        help = "Database URL"
    )]
    pub database_url: String,

    #[arg(
        long,
        env = "DATABASE_MAX_CONNECTIONS",
        default_value = "10",
        help = "Maximum database connections"
    )]
    pub database_max_connections: u32,

    #[arg(
        long,
        env = "DATABASE_CONNECT_TIMEOUT_SECONDS",
        default_value = "30",
        help = "Database connection timeout in seconds"
    )]
    pub database_connect_timeout_seconds: u64,

    #[arg(
        long,
        env = "FLASHBLOCKS_WEBSOCKET_URLS",
        value_delimiter = ',',
        help = "Comma-separated list of builder WebSocket URLs"
    )]
    pub builder_urls: Vec<String>,

    #[arg(
        long,
        env = "FLASHBLOCKS_RECONNECT_DELAY_SECONDS",
        default_value = "5",
        help = "WebSocket reconnect delay in seconds"
    )]
    pub reconnect_delay_seconds: u64,

    #[arg(
        long,
        env = "ARCHIVER_BUFFER_SIZE",
        default_value = "1000",
        help = "Message buffer size"
    )]
    pub buffer_size: usize,

    #[arg(
        long,
        env = "ARCHIVER_BATCH_SIZE",
        default_value = "100",
        help = "Batch size for database writes"
    )]
    pub batch_size: usize,

    #[arg(
        long,
        env = "ARCHIVER_FLUSH_INTERVAL_SECONDS",
        default_value = "5",
        help = "Flush interval in seconds"
    )]
    pub flush_interval_seconds: u64,
}

#[derive(Debug, Clone)]
pub struct BuilderConfig {
    pub name: String,
    pub url: Url,
    pub reconnect_delay_seconds: u64,
}

impl FlashblocksArchiverArgs {
    pub fn parse_builders(&self) -> Result<Vec<BuilderConfig>> {
        self.builder_urls
            .iter()
            .enumerate()
            .map(|(i, url)| {
                Ok(BuilderConfig {
                    name: format!("builder_{}", i),
                    url: Url::parse(url.trim())?,
                    reconnect_delay_seconds: self.reconnect_delay_seconds,
                })
            })
            .collect::<Result<Vec<_>>>()
    }
}
