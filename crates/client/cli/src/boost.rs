//! Rollup Boost CLI Args

use kona_engine::RollupBoostServerArgs;
use rollup_boost_kona::{BlockSelectionPolicy, ExecutionMode};
use url::Url;

const DEFAULT_FLASHBLOCKS_BUILDER_URL: &str = "ws://localhost:1111";
const DEFAULT_FLASHBLOCKS_HOST: &str = "localhost";
const DEFAULT_FLASHBLOCKS_PORT: u16 = 1112;

const DEFAULT_FLASHBLOCKS_BUILDER_WS_INITIAL_RECONNECT_MS: u64 = 10;
const DEFAULT_FLASHBLOCKS_BUILDER_WS_MAX_RECONNECT_MS: u64 = 5000;
const DEFAULT_FLASHBLOCKS_BUILDER_WS_PING_INTERVAL_MS: u64 = 500;
const DEFAULT_FLASHBLOCKS_BUILDER_WS_PONG_TIMEOUT_MS: u64 = 1500;

/// Custom block builder flags.
#[derive(Clone, Debug, clap::Args)]
pub struct RollupBoostFlags {
    /// Execution mode to start rollup boost with
    #[arg(
        long,
        visible_alias = "rollup-boost.execution-mode",
        env = "BASE_NODE_ROLLUP_BOOST_EXECUTION_MODE",
        default_value = "disabled"
    )]
    pub execution_mode: ExecutionMode,

    /// Block selection policy to use for the rollup boost server.
    #[arg(
        long,
        visible_alias = "rollup-boost.block-selection-policy",
        env = "BASE_NODE_ROLLUP_BOOST_BLOCK_SELECTION_POLICY"
    )]
    pub block_selection_policy: Option<BlockSelectionPolicy>,

    /// Should we use the l2 client for computing state root
    #[arg(
        long,
        visible_alias = "rollup-boost.external-state-root",
        env = "BASE_NODE_ROLLUP_BOOST_EXTERNAL_STATE_ROOT",
        default_value = "false"
    )]
    pub external_state_root: bool,

    /// Allow all engine API calls to builder even when marked as unhealthy
    /// This is default true assuming no builder CL set up
    #[arg(
        long,
        visible_alias = "rollup-boost.ignore-unhealthy-builders",
        env = "BASE_NODE_ROLLUP_BOOST_IGNORE_UNHEALTHY_BUILDERS",
        default_value = "false"
    )]
    pub ignore_unhealthy_builders: bool,

    /// Duration in seconds between async health checks on the builder
    #[arg(
        long,
        visible_alias = "rollup-boost.health-check-interval",
        env = "BASE_NODE_ROLLUP_BOOST_HEALTH_CHECK_INTERVAL",
        default_value = "60"
    )]
    pub health_check_interval: u64,

    /// Max duration in seconds between the unsafe head block of the builder and the current time
    #[arg(
        long,
        visible_alias = "rollup-boost.max-unsafe-interval",
        env = "BASE_NODE_ROLLUP_BOOST_MAX_UNSAFE_INTERVAL",
        default_value = "10"
    )]
    pub max_unsafe_interval: u64,

    /// Flashblocks configuration
    #[clap(flatten)]
    pub flashblocks: FlashblocksFlags,
}

impl Default for RollupBoostFlags {
    fn default() -> Self {
        Self {
            execution_mode: ExecutionMode::Disabled,
            block_selection_policy: None,
            external_state_root: false,
            ignore_unhealthy_builders: false,
            flashblocks: FlashblocksFlags::default(),
            health_check_interval: 60,
            max_unsafe_interval: 10,
        }
    }
}

impl RollupBoostFlags {
    /// Converts the rollup boost cli arguments to the rollup boost arguments used by the engine.
    pub fn as_rollup_boost_args(self) -> RollupBoostServerArgs {
        RollupBoostServerArgs {
            initial_execution_mode: self.execution_mode,
            block_selection_policy: self.block_selection_policy,
            external_state_root: self.external_state_root,
            ignore_unhealthy_builders: self.ignore_unhealthy_builders,
            flashblocks: self.flashblocks.flashblocks.then_some(
                kona_engine::FlashblocksClientArgs {
                    flashblocks_builder_url: self.flashblocks.flashblocks_builder_url,
                    flashblocks_host: self.flashblocks.flashblocks_host,
                    flashblocks_port: self.flashblocks.flashblocks_port,
                    flashblocks_ws_config: kona_engine::FlashblocksWebsocketConfig {
                        flashblock_builder_ws_initial_reconnect_ms: self
                            .flashblocks
                            .flashblocks_ws_config
                            .flashblock_builder_ws_initial_reconnect_ms,
                        flashblock_builder_ws_max_reconnect_ms: self
                            .flashblocks
                            .flashblocks_ws_config
                            .flashblock_builder_ws_max_reconnect_ms,
                        flashblock_builder_ws_ping_interval_ms: self
                            .flashblocks
                            .flashblocks_ws_config
                            .flashblock_builder_ws_ping_interval_ms,
                        flashblock_builder_ws_pong_timeout_ms: self
                            .flashblocks
                            .flashblocks_ws_config
                            .flashblock_builder_ws_pong_timeout_ms,
                    },
                },
            ),
        }
    }
}

/// Flashblocks flags.
#[derive(Clone, Debug, clap::Args)]
pub struct FlashblocksFlags {
    /// Enable Flashblocks client
    #[arg(
        long,
        visible_alias = "rollup-boost.flashblocks",
        env = "BASE_NODE_FLASHBLOCKS",
        default_value = "false"
    )]
    pub flashblocks: bool,

    /// Flashblocks Builder WebSocket URL
    #[arg(
        long,
        visible_alias = "rollup-boost.flashblocks-builder-url",
        env = "BASE_NODE_FLASHBLOCKS_BUILDER_URL",
        default_value = DEFAULT_FLASHBLOCKS_BUILDER_URL
    )]
    pub flashblocks_builder_url: Url,

    /// Flashblocks WebSocket host for outbound connections
    #[arg(
        long,
        visible_alias = "rollup-boost.flashblocks-host",
        env = "BASE_NODE_FLASHBLOCKS_HOST",
        default_value = DEFAULT_FLASHBLOCKS_HOST
    )]
    pub flashblocks_host: String,

    /// Flashblocks WebSocket port for outbound connections
    #[arg(
        long,
        visible_alias = "rollup-boost.flashblocks-port",
        env = "BASE_NODE_FLASHBLOCKS_PORT",
        default_value_t = DEFAULT_FLASHBLOCKS_PORT
    )]
    pub flashblocks_port: u16,

    /// Websocket connection configuration
    #[command(flatten)]
    pub flashblocks_ws_config: FlashblocksWebsocketFlags,
}

impl Default for FlashblocksFlags {
    fn default() -> Self {
        Self {
            flashblocks: false,
            flashblocks_builder_url: Url::parse(DEFAULT_FLASHBLOCKS_BUILDER_URL).unwrap(),
            flashblocks_host: DEFAULT_FLASHBLOCKS_HOST.to_string(),
            flashblocks_port: DEFAULT_FLASHBLOCKS_PORT,
            flashblocks_ws_config: FlashblocksWebsocketFlags::default(),
        }
    }
}

/// Configuration for the Flashblocks WebSocket connection.
#[derive(clap::Parser, Debug, Clone, Copy)]
pub struct FlashblocksWebsocketFlags {
    /// Minimum time for exponential backoff for timeout if builder disconnected
    #[arg(
        long,
        visible_alias = "rollup-boost.flashblocks-initial-reconnect-ms",
        env = "BASE_NODE_FLASHBLOCKS_BUILDER_WS_INITIAL_RECONNECT_MS",
        default_value_t = DEFAULT_FLASHBLOCKS_BUILDER_WS_INITIAL_RECONNECT_MS
    )]
    pub flashblock_builder_ws_initial_reconnect_ms: u64,

    /// Maximum time for exponential backoff for timeout if builder disconnected
    #[arg(
        long,
        visible_alias = "rollup-boost.flashblocks-max-reconnect-ms",
        env = "BASE_NODE_FLASHBLOCKS_BUILDER_WS_MAX_RECONNECT_MS",
        default_value_t = DEFAULT_FLASHBLOCKS_BUILDER_WS_MAX_RECONNECT_MS
    )]
    pub flashblock_builder_ws_max_reconnect_ms: u64,

    /// Interval in milliseconds between ping messages sent to upstream servers to detect
    /// unresponsive connections
    #[arg(
        long,
        visible_alias = "rollup-boost.flashblocks-ping-interval-ms",
        env = "BASE_NODE_FLASHBLOCKS_BUILDER_WS_PING_INTERVAL_MS",
        default_value_t = DEFAULT_FLASHBLOCKS_BUILDER_WS_PING_INTERVAL_MS
    )]
    pub flashblock_builder_ws_ping_interval_ms: u64,

    /// Timeout in milliseconds to wait for pong responses from upstream servers before considering
    /// the connection dead
    #[arg(
        long,
        visible_alias = "rollup-boost.flashblocks-pong-timeout-ms",
        env = "BASE_NODE_FLASHBLOCKS_BUILDER_WS_PONG_TIMEOUT_MS",
        default_value_t = DEFAULT_FLASHBLOCKS_BUILDER_WS_PONG_TIMEOUT_MS
    )]
    pub flashblock_builder_ws_pong_timeout_ms: u64,
}

impl Default for FlashblocksWebsocketFlags {
    fn default() -> Self {
        Self {
            flashblock_builder_ws_initial_reconnect_ms:
                DEFAULT_FLASHBLOCKS_BUILDER_WS_INITIAL_RECONNECT_MS,
            flashblock_builder_ws_max_reconnect_ms: DEFAULT_FLASHBLOCKS_BUILDER_WS_MAX_RECONNECT_MS,
            flashblock_builder_ws_ping_interval_ms: DEFAULT_FLASHBLOCKS_BUILDER_WS_PING_INTERVAL_MS,
            flashblock_builder_ws_pong_timeout_ms: DEFAULT_FLASHBLOCKS_BUILDER_WS_PONG_TIMEOUT_MS,
        }
    }
}
