//! CLI argument definitions for the devnet binary.

use std::path::PathBuf;

use clap::Parser;

/// Base Devnet - launches a local base-builder and base-client network for testing.
#[derive(Debug, Clone, Parser)]
#[command(name = "base-devnet", about = "Launch a local Base devnet for testing")]
pub(crate) struct DevnetArgs {
    /// Data directory for devnet state.
    ///
    /// Stores blockchain data for both builder and client nodes.
    /// If not specified, uses a temporary directory.
    #[arg(long, short = 'd', value_name = "PATH")]
    pub datadir: Option<PathBuf>,

    /// HTTP RPC port for the client node.
    #[arg(long, default_value = "8545")]
    pub http_port: u16,

    /// WebSocket RPC port for the client node.
    #[arg(long, default_value = "8546")]
    pub ws_port: u16,

    /// Engine API port for consensus layer communication.
    #[arg(long, default_value = "8551")]
    pub engine_port: u16,

    /// Builder HTTP RPC port.
    #[arg(long, default_value = "8645")]
    pub builder_http_port: u16,

    /// Builder Engine API port.
    #[arg(long, default_value = "8651")]
    pub builder_engine_port: u16,

    /// Metrics port for the client node.
    #[arg(long, default_value = "9001")]
    pub metrics_port: u16,

    /// Builder metrics port.
    #[arg(long, default_value = "9002")]
    pub builder_metrics_port: u16,

    /// P2P port for the client node.
    #[arg(long, default_value = "30303")]
    pub p2p_port: u16,

    /// P2P port for the builder node.
    #[arg(long, default_value = "30304")]
    pub builder_p2p_port: u16,

    /// Block time in milliseconds.
    #[arg(long, default_value = "2000")]
    pub block_time_ms: u64,

    /// Enable flashblocks support.
    #[arg(long, default_value = "true")]
    pub flashblocks: bool,

    /// Log level (error, warn, info, debug, trace).
    #[arg(long, short = 'v', default_value = "info")]
    pub log_level: String,

    /// Don't start the builder (client-only mode).
    #[arg(long)]
    pub no_builder: bool,

    /// Don't start the client (builder-only mode).
    #[arg(long)]
    pub no_client: bool,

    /// Don't start the block driver (for plugging in external consensus).
    ///
    /// Use this when you want to connect your own consensus client (op-node, Kona, etc.)
    /// to drive block production instead of the built-in driver.
    #[arg(long)]
    pub no_driver: bool,

    /// Disable terminal UI mode (use plain console logging).
    #[cfg(feature = "tui")]
    #[arg(long)]
    pub no_tui: bool,
}

impl DevnetArgs {
    /// Returns the data directory, creating a temp dir if not specified.
    pub(crate) fn data_dir(&self) -> PathBuf {
        self.datadir.clone().unwrap_or_else(|| {
            let temp = tempfile::tempdir().expect("failed to create temp directory");
            let path = temp.path().to_path_buf();
            // Keep the temp directory so it doesn't get deleted
            std::mem::forget(temp);
            path
        })
    }

    /// Returns the client data directory.
    pub(crate) fn client_datadir(&self) -> PathBuf {
        self.data_dir().join("client")
    }

    /// Returns the builder data directory.
    pub(crate) fn builder_datadir(&self) -> PathBuf {
        self.data_dir().join("builder")
    }
}
