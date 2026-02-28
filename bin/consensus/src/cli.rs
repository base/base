//! Contains the CLI entry point for the Base consensus binary.

use std::{
    net::IpAddr,
    num::ParseIntError,
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

use alloy_chains::Chain;
use alloy_primitives::{Address, B256};
use alloy_rpc_types_engine::JwtSecret;
use base_cli_utils::{CliStyles, LogConfig, RuntimeManager};
use base_client_cli::{
    L1ClientArgs, L1ConfigFile, L2ClientArgs, L2ConfigFile, P2PArgs, RpcArgs, SequencerArgs,
    SignerArgs,
};
use base_client_cli::p2p::resolve_host;
use base_consensus_node::{EngineConfig, L1ConfigBuilder, NodeMode, RollupNodeBuilder};
use base_consensus_peers::PeerScoreLevel;
use base_consensus_registry::Registry;
use clap::Parser;
use strum::IntoEnumIterator;
use tracing::{error, info};
use url::Url;

use crate::metrics::init_rollup_config_metrics;

base_cli_utils::define_log_args!("BASE_NODE");
base_cli_utils::define_metrics_args!("BASE_NODE", 9090);

// ── Clap wrapper: L1 client args ──────────────────────────────────────────

/// CLI arguments for the L1 client.
#[derive(Clone, Debug, clap::Args)]
pub struct CliL1Args {
    /// URL of the L1 execution client RPC API.
    #[arg(long, visible_alias = "l1", env = "BASE_NODE_L1_ETH_RPC")]
    pub l1_eth_rpc: Url,
    /// Whether to trust the L1 RPC.
    /// If false, block hash verification is performed for all retrieved blocks.
    #[arg(
        long,
        visible_alias = "l1.trust-rpc",
        env = "BASE_NODE_L1_TRUST_RPC",
        default_value_t = true
    )]
    pub l1_trust_rpc: bool,
    /// URL of the L1 beacon API.
    #[arg(long, visible_alias = "l1.beacon", env = "BASE_NODE_L1_BEACON")]
    pub l1_beacon: Url,
    /// Duration in seconds of an L1 slot.
    ///
    /// This is an optional argument that can be used to use a fixed slot duration for l1 blocks
    /// and bypass the initial beacon spec fetch. This is useful for testing purposes when the
    /// l1-beacon spec endpoint is not available (with anvil for example).
    #[arg(
        long,
        visible_alias = "l1.slot-duration-override",
        env = "BASE_NODE_L1_SLOT_DURATION_OVERRIDE"
    )]
    pub l1_slot_duration_override: Option<u64>,
}

impl From<CliL1Args> for L1ClientArgs {
    fn from(cli: CliL1Args) -> Self {
        Self {
            l1_eth_rpc: cli.l1_eth_rpc,
            l1_trust_rpc: cli.l1_trust_rpc,
            l1_beacon: cli.l1_beacon,
            l1_slot_duration_override: cli.l1_slot_duration_override,
        }
    }
}

// ── Clap wrapper: L2 client args ──────────────────────────────────────────

/// CLI arguments for the L2 engine client.
#[derive(Clone, Debug, clap::Args)]
pub struct CliL2Args {
    /// URI of the engine API endpoint of an L2 execution client.
    #[arg(long, visible_alias = "l2", env = "BASE_NODE_L2_ENGINE_RPC")]
    pub l2_engine_rpc: Url,
    /// JWT secret for the auth-rpc endpoint of the execution client.
    /// This MUST be a valid path to a file containing the hex-encoded JWT secret.
    #[arg(long, visible_alias = "l2.jwt-secret", env = "BASE_NODE_L2_ENGINE_AUTH")]
    pub l2_engine_jwt_secret: Option<PathBuf>,
    /// Hex encoded JWT secret to use for the authenticated engine-API RPC server.
    /// This MUST be a valid hex-encoded JWT secret of 64 digits.
    #[arg(long, visible_alias = "l2.jwt-secret-encoded", env = "BASE_NODE_L2_ENGINE_AUTH_ENCODED")]
    pub l2_engine_jwt_encoded: Option<JwtSecret>,
    /// Timeout for http calls in milliseconds.
    #[arg(
        long,
        visible_alias = "l2.timeout",
        env = "BASE_NODE_L2_ENGINE_TIMEOUT",
        default_value_t = 30_000
    )]
    pub l2_engine_timeout: u64,
    /// If false, block hash verification is performed for all retrieved blocks.
    #[arg(
        long,
        visible_alias = "l2.trust-rpc",
        env = "BASE_NODE_L2_TRUST_RPC",
        default_value_t = true
    )]
    pub l2_trust_rpc: bool,
}

impl From<CliL2Args> for L2ClientArgs {
    fn from(cli: CliL2Args) -> Self {
        Self {
            l2_engine_rpc: cli.l2_engine_rpc,
            l2_engine_jwt_secret: cli.l2_engine_jwt_secret,
            l2_engine_jwt_encoded: cli.l2_engine_jwt_encoded,
            l2_engine_timeout: cli.l2_engine_timeout,
            l2_trust_rpc: cli.l2_trust_rpc,
        }
    }
}

// ── Clap wrapper: L1 config file ──────────────────────────────────────────

/// CLI argument for the L1 chain configuration file path.
#[derive(Clone, Debug, Default, clap::Args)]
pub struct CliL1ConfigFile {
    /// Path to a custom L1 chain configuration file.
    /// (overrides the default configuration from the registry)
    #[arg(long, visible_alias = "rollup-l1-cfg", env = "BASE_NODE_L1_CHAIN_CONFIG")]
    pub l1_config_file: Option<PathBuf>,
}

impl From<CliL1ConfigFile> for L1ConfigFile {
    fn from(cli: CliL1ConfigFile) -> Self {
        Self::new(cli.l1_config_file)
    }
}

// ── Clap wrapper: L2 config file ──────────────────────────────────────────

/// CLI argument for the L2 rollup configuration file path.
#[derive(Clone, Debug, Default, clap::Args)]
pub struct CliL2ConfigFile {
    /// Path to a custom L2 rollup configuration file.
    /// (overrides the default rollup configuration from the registry)
    #[arg(long, visible_alias = "rollup-cfg", env = "BASE_NODE_ROLLUP_CONFIG")]
    pub l2_config_file: Option<PathBuf>,
}

impl From<CliL2ConfigFile> for L2ConfigFile {
    fn from(cli: CliL2ConfigFile) -> Self {
        Self::new(cli.l2_config_file)
    }
}

// ── Clap wrapper: RPC args ────────────────────────────────────────────────

/// CLI arguments for the RPC server.
#[derive(Debug, Clone, clap::Args)]
pub struct CliRpcArgs {
    /// Whether to disable the rpc server.
    #[arg(long = "rpc.disabled", default_value = "false", env = "BASE_NODE_RPC_DISABLED")]
    pub rpc_disabled: bool,
    /// Prevent the RPC server from attempting to restart.
    #[arg(long = "rpc.no-restart", default_value = "false", env = "BASE_NODE_RPC_NO_RESTART")]
    pub no_restart: bool,
    /// RPC listening address.
    #[arg(long = "rpc.addr", default_value = "0.0.0.0", env = "BASE_NODE_RPC_ADDR")]
    pub listen_addr: IpAddr,
    /// RPC listening port.
    #[arg(long = "port", alias = "rpc.port", default_value = "9545", env = "BASE_NODE_RPC_PORT")]
    pub listen_port: u16,
    /// Enable the admin API.
    #[arg(long = "rpc.enable-admin", env = "BASE_NODE_RPC_ENABLE_ADMIN")]
    pub enable_admin: bool,
    /// File path used to persist state changes made via the admin API so they persist across
    /// restarts. Disabled if not set.
    #[arg(long = "rpc.admin-state", env = "BASE_NODE_RPC_ADMIN_STATE")]
    pub admin_persistence: Option<PathBuf>,
    /// Enables websocket rpc server to track block production.
    #[arg(long = "rpc.ws-enabled", default_value = "false", env = "BASE_NODE_RPC_WS_ENABLED")]
    pub ws_enabled: bool,
    /// Enables development RPC endpoints for engine state introspection.
    #[arg(long = "rpc.dev-enabled", default_value = "false", env = "BASE_NODE_RPC_DEV_ENABLED")]
    pub dev_enabled: bool,
}

impl From<CliRpcArgs> for RpcArgs {
    fn from(cli: CliRpcArgs) -> Self {
        Self {
            rpc_disabled: cli.rpc_disabled,
            no_restart: cli.no_restart,
            listen_addr: cli.listen_addr,
            listen_port: cli.listen_port,
            enable_admin: cli.enable_admin,
            admin_persistence: cli.admin_persistence,
            ws_enabled: cli.ws_enabled,
            dev_enabled: cli.dev_enabled,
        }
    }
}

// ── Clap wrapper: Sequencer args ──────────────────────────────────────────

/// CLI arguments for the sequencer.
#[derive(Clone, Debug, clap::Args)]
pub struct CliSequencerArgs {
    /// Initialize the sequencer in a stopped state. The sequencer can be started using the
    /// `admin_startSequencer` RPC.
    #[arg(
        long = "sequencer.stopped",
        default_value = "false",
        env = "BASE_NODE_SEQUENCER_STOPPED"
    )]
    pub stopped: bool,

    /// Maximum number of L2 blocks for restricting the distance between L2 safe and unsafe.
    /// Disabled if 0.
    #[arg(
        long = "sequencer.max-safe-lag",
        default_value = "0",
        env = "BASE_NODE_SEQUENCER_MAX_SAFE_LAG"
    )]
    pub max_safe_lag: u64,

    /// Number of L1 blocks to keep distance from the L1 head as a sequencer for picking an L1
    /// origin.
    #[arg(long = "sequencer.l1-confs", default_value = "4", env = "BASE_NODE_SEQUENCER_L1_CONFS")]
    pub l1_confs: u64,

    /// Forces the sequencer to strictly prepare the next L1 origin and create empty L2 blocks.
    #[arg(
        long = "sequencer.recover",
        default_value = "false",
        env = "BASE_NODE_SEQUENCER_RECOVER"
    )]
    pub recover: bool,

    /// Conductor service rpc endpoint. Providing this value will enable the conductor service.
    #[arg(long = "conductor.rpc", env = "BASE_NODE_CONDUCTOR_RPC")]
    pub conductor_rpc: Option<Url>,

    /// Conductor service rpc timeout.
    #[arg(
        long = "conductor.rpc.timeout",
        default_value = "1",
        env = "BASE_NODE_CONDUCTOR_RPC_TIMEOUT",
        value_parser = |arg: &str| -> Result<Duration, ParseIntError> {Ok(Duration::from_secs(arg.parse()?))}
    )]
    pub conductor_rpc_timeout: Duration,
}

impl From<CliSequencerArgs> for SequencerArgs {
    fn from(cli: CliSequencerArgs) -> Self {
        Self {
            stopped: cli.stopped,
            max_safe_lag: cli.max_safe_lag,
            l1_confs: cli.l1_confs,
            recover: cli.recover,
            conductor_rpc: cli.conductor_rpc,
            conductor_rpc_timeout: cli.conductor_rpc_timeout,
        }
    }
}

// ── Clap wrapper: Signer args ─────────────────────────────────────────────

/// CLI arguments for the block signer configuration.
#[derive(Debug, Clone, Default, clap::Args)]
pub struct CliSignerArgs {
    /// An optional flag to specify a local private key for the sequencer to sign unsafe blocks.
    #[arg(
        long = "p2p.sequencer.key",
        env = "BASE_NODE_P2P_SEQUENCER_KEY",
        conflicts_with = "endpoint"
    )]
    pub sequencer_key: Option<B256>,
    /// An optional path to a file containing the sequencer private key.
    /// This is mutually exclusive with `p2p.sequencer.key`.
    #[arg(
        long = "p2p.sequencer.key.path",
        env = "BASE_NODE_P2P_SEQUENCER_KEY_PATH",
        conflicts_with = "sequencer_key"
    )]
    pub sequencer_key_path: Option<PathBuf>,
    /// The URL of the remote signer endpoint. If not provided, remote signer will be disabled.
    /// This is mutually exclusive with `p2p.sequencer.key`.
    /// This is required if any of the other signer flags are provided.
    #[arg(
        long = "p2p.signer.endpoint",
        env = "BASE_NODE_P2P_SIGNER_ENDPOINT",
        requires = "address"
    )]
    pub endpoint: Option<Url>,
    /// The address to sign transactions for. Required if `signer.endpoint` is provided.
    #[arg(
        long = "p2p.signer.address",
        env = "BASE_NODE_P2P_SIGNER_ADDRESS",
        requires = "endpoint"
    )]
    pub address: Option<Address>,
    /// Headers to pass to the remote signer. Format `key=value`. Value can contain any character
    /// allowed in a HTTP header. When using env vars, split with commas. When using flags one
    /// key value pair per flag.
    #[arg(long = "p2p.signer.header", env = "BASE_NODE_P2P_SIGNER_HEADER", requires = "endpoint")]
    pub header: Vec<String>,
    /// An optional path to CA certificates to be used for the remote signer.
    #[arg(long = "p2p.signer.tls.ca", env = "BASE_NODE_P2P_SIGNER_TLS_CA", requires = "endpoint")]
    pub ca_cert: Option<PathBuf>,
    /// An optional path to the client certificate for the remote signer. If specified,
    /// `signer.tls.key` must also be specified.
    #[arg(
        long = "p2p.signer.tls.cert",
        env = "BASE_NODE_P2P_SIGNER_TLS_CERT",
        requires = "key",
        requires = "endpoint"
    )]
    pub cert: Option<PathBuf>,
    /// An optional path to the client key for the remote signer. If specified,
    /// `signer.tls.cert` must also be specified.
    #[arg(
        long = "p2p.signer.tls.key",
        env = "BASE_NODE_P2P_SIGNER_TLS_KEY",
        requires = "cert",
        requires = "endpoint"
    )]
    pub key: Option<PathBuf>,
}

impl From<CliSignerArgs> for SignerArgs {
    fn from(cli: CliSignerArgs) -> Self {
        Self {
            sequencer_key: cli.sequencer_key,
            sequencer_key_path: cli.sequencer_key_path,
            endpoint: cli.endpoint,
            address: cli.address,
            header: cli.header,
            ca_cert: cli.ca_cert,
            cert: cli.cert,
            key: cli.key,
        }
    }
}

// ── Clap wrapper: P2P args ────────────────────────────────────────────────

/// CLI arguments for the P2P network layer.
#[derive(Clone, Debug, clap::Args)]
pub struct CliP2PArgs {
    /// Disable Discv5 (node discovery).
    #[arg(long = "p2p.no-discovery", default_value = "false", env = "BASE_NODE_P2P_NO_DISCOVERY")]
    pub no_discovery: bool,
    /// Read the hex-encoded 32-byte private key for the peer ID from this txt file.
    /// Created if not already exists. Important to persist to keep the same network identity after
    /// restarting, maintaining the previous advertised identity.
    #[arg(long = "p2p.priv.path", env = "BASE_NODE_P2P_PRIV_PATH")]
    pub priv_path: Option<PathBuf>,
    /// The hex-encoded 32-byte private key for the peer ID.
    #[arg(long = "p2p.priv.raw", env = "BASE_NODE_P2P_PRIV_RAW")]
    pub private_key: Option<B256>,

    /// IP address or DNS hostname to advertise to external peers from Discv5.
    /// Optional argument. Use the `p2p.listen.ip` if not set.
    /// Accepts either an IP address (e.g., "1.2.3.4") or a DNS hostname (e.g.,
    /// "node1.example.com"). DNS hostnames are resolved to IP addresses at startup.
    ///
    /// Technical note: if this argument is set, the dynamic ENR updates from the discovery layer
    /// will be disabled. This is to allow the advertised IP to be static (to use in a network
    /// behind a NAT for instance).
    #[arg(long = "p2p.advertise.ip", env = "BASE_NODE_P2P_ADVERTISE_IP", value_parser = resolve_host)]
    pub advertise_ip: Option<IpAddr>,
    /// TCP port to advertise to external peers from the discovery layer. Same as `p2p.listen.tcp`
    /// if set to zero.
    #[arg(long = "p2p.advertise.tcp", env = "BASE_NODE_P2P_ADVERTISE_TCP_PORT")]
    pub advertise_tcp_port: Option<u16>,
    /// UDP port to advertise to external peers from the discovery layer.
    /// Same as `p2p.listen.udp` if set to zero.
    #[arg(long = "p2p.advertise.udp", env = "BASE_NODE_P2P_ADVERTISE_UDP_PORT")]
    pub advertise_udp_port: Option<u16>,

    /// IP address or DNS hostname to bind LibP2P/Discv5 to.
    /// Accepts either an IP address (e.g., "0.0.0.0") or a DNS hostname (e.g.,
    /// "node1.example.com"). DNS hostnames are resolved to IP addresses at startup.
    #[arg(long = "p2p.listen.ip", default_value = "0.0.0.0", env = "BASE_NODE_P2P_LISTEN_IP", value_parser = resolve_host)]
    pub listen_ip: IpAddr,
    /// TCP port to bind `LibP2P` to. Any available system port if set to 0.
    #[arg(long = "p2p.listen.tcp", default_value = "9222", env = "BASE_NODE_P2P_LISTEN_TCP_PORT")]
    pub listen_tcp_port: u16,
    /// UDP port to bind Discv5 to. Same as TCP port if left 0.
    #[arg(long = "p2p.listen.udp", default_value = "9223", env = "BASE_NODE_P2P_LISTEN_UDP_PORT")]
    pub listen_udp_port: u16,
    /// Low-tide peer count. The node actively searches for new peer connections if below this
    /// amount.
    #[arg(long = "p2p.peers.lo", default_value = "20", env = "BASE_NODE_P2P_PEERS_LO")]
    pub peers_lo: u32,
    /// High-tide peer count. The node starts pruning peer connections slowly after reaching this
    /// number.
    #[arg(long = "p2p.peers.hi", default_value = "30", env = "BASE_NODE_P2P_PEERS_HI")]
    pub peers_hi: u32,
    /// Grace period to keep a newly connected peer around, if it is not misbehaving.
    #[arg(
        long = "p2p.peers.grace",
        default_value = "30",
        env = "BASE_NODE_P2P_PEERS_GRACE",
        value_parser = |arg: &str| -> Result<Duration, ParseIntError> {Ok(Duration::from_secs(arg.parse()?))}
    )]
    pub peers_grace: Duration,
    /// Configure `GossipSub` topic stable mesh target count.
    /// Aka: The desired outbound degree (numbers of peers to gossip to).
    #[arg(long = "p2p.gossip.mesh.d", default_value = "8", env = "BASE_NODE_P2P_GOSSIP_MESH_D")]
    pub gossip_mesh_d: usize,
    /// Configure `GossipSub` topic stable mesh low watermark.
    /// Aka: The lower bound of outbound degree.
    #[arg(long = "p2p.gossip.mesh.lo", default_value = "6", env = "BASE_NODE_P2P_GOSSIP_MESH_DLO")]
    pub gossip_mesh_dlo: usize,
    /// Configure `GossipSub` topic stable mesh high watermark.
    /// Aka: The upper bound of outbound degree (additional peers will not receive gossip).
    #[arg(
        long = "p2p.gossip.mesh.dhi",
        default_value = "12",
        env = "BASE_NODE_P2P_GOSSIP_MESH_DHI"
    )]
    pub gossip_mesh_dhi: usize,
    /// Configure `GossipSub` gossip target.
    /// Aka: The target degree for gossip only (not messaging like p2p.gossip.mesh.d, just
    /// announcements of IHAVE).
    #[arg(
        long = "p2p.gossip.mesh.dlazy",
        default_value = "6",
        env = "BASE_NODE_P2P_GOSSIP_MESH_DLAZY"
    )]
    pub gossip_mesh_dlazy: usize,
    /// Configure `GossipSub` to publish messages to all known peers on the topic, outside of the
    /// mesh. Also see Dlazy as less aggressive alternative.
    #[arg(
        long = "p2p.gossip.mesh.floodpublish",
        default_value = "false",
        env = "BASE_NODE_P2P_GOSSIP_FLOOD_PUBLISH"
    )]
    pub gossip_flood_publish: bool,
    /// Sets the peer scoring strategy for the P2P stack.
    /// Can be one of: none or light.
    #[arg(long = "p2p.scoring", default_value = "light", env = "BASE_NODE_P2P_SCORING")]
    pub scoring: PeerScoreLevel,

    /// Allows to ban peers based on their score.
    ///
    /// Peers are banned based on a ban threshold (see `p2p.ban.threshold`).
    /// If a peer's score is below the threshold, it gets automatically banned.
    #[arg(long = "p2p.ban.peers", default_value = "false", env = "BASE_NODE_P2P_BAN_PEERS")]
    pub ban_enabled: bool,

    /// The threshold used to ban peers.
    ///
    /// For peers to be banned, the `p2p.ban.peers` flag must be set to `true`.
    /// By default, peers are banned if their score is below -100.
    #[arg(long = "p2p.ban.threshold", default_value = "-100", env = "BASE_NODE_P2P_BAN_THRESHOLD")]
    pub ban_threshold: i64,

    /// The duration in minutes to ban a peer for.
    ///
    /// For peers to be banned, the `p2p.ban.peers` flag must be set to `true`.
    /// By default peers are banned for 1 hour.
    #[arg(long = "p2p.ban.duration", default_value = "60", env = "BASE_NODE_P2P_BAN_DURATION")]
    pub ban_duration: u64,

    /// The interval in seconds to find peers using the discovery service.
    /// Defaults to 5 seconds.
    #[arg(
        long = "p2p.discovery.interval",
        default_value = "5",
        env = "BASE_NODE_P2P_DISCOVERY_INTERVAL"
    )]
    pub discovery_interval: u64,
    /// The directory to store the bootstore.
    #[arg(long = "p2p.bootstore", env = "BASE_NODE_P2P_BOOTSTORE")]
    pub bootstore: Option<PathBuf>,
    /// Disables the bootstore.
    #[arg(long = "p2p.no-bootstore", env = "BASE_NODE_P2P_NO_BOOTSTORE")]
    pub disable_bootstore: bool,
    /// Peer Redialing threshold is the maximum amount of times to attempt to redial a peer that
    /// disconnects. By default, peers are *not* redialed. If set to 0, the peer will be
    /// redialed indefinitely.
    #[arg(long = "p2p.redial", env = "BASE_NODE_P2P_REDIAL", default_value = "500")]
    pub peer_redial: Option<u64>,

    /// The duration in minutes of the peer dial period.
    /// When the last time a peer was dialed is longer than the dial period, the number of peer
    /// dials is reset to 0, allowing the peer to be dialed again.
    #[arg(long = "p2p.redial.period", env = "BASE_NODE_P2P_REDIAL_PERIOD", default_value = "60")]
    pub redial_period: u64,

    /// An optional list of bootnode ENRs or node records to start the node with.
    #[arg(long = "p2p.bootnodes", value_delimiter = ',', env = "BASE_NODE_P2P_BOOTNODES")]
    pub bootnodes: Vec<String>,

    /// Optionally enable topic scoring.
    ///
    /// Topic scoring is a mechanism to score peers based on their behavior in the gossip network.
    /// This flag is only presented for backwards compatibility and debugging purposes.
    #[arg(
        long = "p2p.topic-scoring",
        default_value = "false",
        env = "BASE_NODE_P2P_TOPIC_SCORING"
    )]
    pub topic_scoring: bool,

    /// An optional unsafe block signer address.
    ///
    /// By default, this is fetched from the chain config in the superchain-registry using the
    /// specified L2 chain ID.
    #[arg(long = "p2p.unsafe.block.signer", env = "BASE_NODE_P2P_UNSAFE_BLOCK_SIGNER")]
    pub unsafe_block_signer: Option<Address>,

    /// An optional flag to remove random peers from discovery to rotate the peer set.
    ///
    /// This is the number of seconds to wait before removing a peer from the discovery
    /// service. By default, peers are not removed from the discovery service.
    ///
    /// This is useful for discovering a wider set of peers.
    #[arg(long = "p2p.discovery.randomize", env = "BASE_NODE_P2P_DISCOVERY_RANDOMIZE")]
    pub discovery_randomize: Option<u64>,

    /// Specify optional remote signer configuration. Note that this argument is mutually exclusive
    /// with `p2p.sequencer.key` that specifies a local sequencer signer.
    #[command(flatten)]
    pub signer: CliSignerArgs,
}

impl From<CliP2PArgs> for P2PArgs {
    fn from(cli: CliP2PArgs) -> Self {
        Self {
            no_discovery: cli.no_discovery,
            priv_path: cli.priv_path,
            private_key: cli.private_key,
            advertise_ip: cli.advertise_ip,
            advertise_tcp_port: cli.advertise_tcp_port,
            advertise_udp_port: cli.advertise_udp_port,
            listen_ip: cli.listen_ip,
            listen_tcp_port: cli.listen_tcp_port,
            listen_udp_port: cli.listen_udp_port,
            peers_lo: cli.peers_lo,
            peers_hi: cli.peers_hi,
            peers_grace: cli.peers_grace,
            gossip_mesh_d: cli.gossip_mesh_d,
            gossip_mesh_dlo: cli.gossip_mesh_dlo,
            gossip_mesh_dhi: cli.gossip_mesh_dhi,
            gossip_mesh_dlazy: cli.gossip_mesh_dlazy,
            gossip_flood_publish: cli.gossip_flood_publish,
            scoring: cli.scoring,
            ban_enabled: cli.ban_enabled,
            ban_threshold: cli.ban_threshold,
            ban_duration: cli.ban_duration,
            discovery_interval: cli.discovery_interval,
            bootstore: cli.bootstore,
            disable_bootstore: cli.disable_bootstore,
            peer_redial: cli.peer_redial,
            redial_period: cli.redial_period,
            bootnodes: cli.bootnodes,
            topic_scoring: cli.topic_scoring,
            unsafe_block_signer: cli.unsafe_block_signer,
            discovery_randomize: cli.discovery_randomize,
            signer: cli.signer.into(),
        }
    }
}

// ── Main CLI struct ───────────────────────────────────────────────────────

/// The Base Consensus CLI.
#[derive(Parser, Clone, Debug)]
#[command(
    author,
    version = env!("CARGO_PKG_VERSION"),
    styles = CliStyles::init(),
    about,
    long_about = None
)]
pub struct Cli {
    /// L2 Chain ID or name (8453 = Base Mainnet, 84532 = Base Sepolia).
    #[arg(
        long = "chain",
        short = 'n',
        global = true,
        default_value = "8453",
        env = "BASE_NODE_NETWORK"
    )]
    pub l2_chain_id: Chain,
    /// Logging configuration.
    #[command(flatten)]
    pub logging: LogArgs,
    /// Metrics configuration.
    #[command(flatten)]
    pub metrics: MetricsArgs,
    /// The mode to run the node in.
    #[arg(
        long = "mode",
        default_value_t = NodeMode::Validator,
        env = "BASE_NODE_MODE",
        help = format!(
            "The mode to run the node in. Supported modes are: {}",
            NodeMode::iter()
                .map(|mode| format!("\"{}\"", mode.to_string()))
                .collect::<Vec<_>>()
                .join(", ")
        )
    )]
    pub node_mode: NodeMode,

    /// L1 RPC CLI arguments.
    #[command(flatten)]
    pub l1_rpc_args: CliL1Args,

    /// L2 engine CLI arguments.
    #[command(flatten)]
    pub l2_client_args: CliL2Args,

    /// L1 configuration file.
    #[command(flatten)]
    pub l1_config: CliL1ConfigFile,
    /// L2 configuration file.
    #[command(flatten)]
    pub l2_config: CliL2ConfigFile,

    /// P2P CLI arguments.
    #[command(flatten)]
    pub p2p_flags: CliP2PArgs,
    /// RPC CLI arguments.
    #[command(flatten)]
    pub rpc_flags: CliRpcArgs,
    /// SEQUENCER CLI arguments.
    #[command(flatten)]
    pub sequencer_flags: CliSequencerArgs,
}

impl Cli {
    /// Runs the CLI.
    pub fn run(self) -> eyre::Result<()> {
        // Initialize logging from global arguments.
        LogConfig::from(self.logging.clone()).init_tracing_subscriber()?;

        // Initialize unified metrics
        base_cli_utils::MetricsConfig::from(self.metrics.clone()).init_with(|| {
            base_consensus_gossip::Metrics::init();
            base_consensus_disc::Metrics::init();
            base_consensus_engine::Metrics::init();
            base_consensus_node::Metrics::init();
            base_consensus_derive::Metrics::init();
            base_consensus_providers::Metrics::init();
            base_cli_utils::register_version_metrics!();
        })?;

        // Run the subcommand.
        RuntimeManager::run_until_ctrl_c(self.exec())
    }

    /// Returns the signer [`Address`] from the rollup config for the given l2 chain id.
    fn genesis_signer(&self) -> eyre::Result<Address> {
        let id = self.l2_chain_id;
        Registry::unsafe_block_signer(id.id())
            .ok_or_else(|| eyre::eyre!("No unsafe block signer found for chain ID: {id}"))
    }

    /// Run the Node subcommand.
    pub async fn exec(&self) -> eyre::Result<()> {
        // Convert clap types to library config types.
        let l1_rpc_args: L1ClientArgs = self.l1_rpc_args.clone().into();
        let l2_client_args: L2ClientArgs = self.l2_client_args.clone().into();
        let l1_config: L1ConfigFile = self.l1_config.clone().into();
        let l2_config: L2ConfigFile = self.l2_config.clone().into();
        let p2p_flags: P2PArgs = self.p2p_flags.clone().into();
        let rpc_flags: RpcArgs = self.rpc_flags.clone().into();
        let sequencer_flags: SequencerArgs = self.sequencer_flags.clone().into();

        let cfg = l2_config.load(&self.l2_chain_id).map_err(|e| eyre::eyre!("{e}"))?;

        info!(
            target: "rollup_node",
            chain_id = cfg.l2_chain_id.id(),
            "Starting rollup node services"
        );
        for hf in cfg.hardforks.to_string().lines() {
            info!(target: "rollup_node", hardfork = %hf, "hardfork");
        }

        let l1_chain_config =
            l1_config.load(cfg.l1_chain_id).map_err(|e| eyre::eyre!("{e}"))?;
        let l1_config_builder = L1ConfigBuilder {
            chain_config: l1_chain_config,
            trust_rpc: l1_rpc_args.l1_trust_rpc,
            beacon: l1_rpc_args.l1_beacon.clone(),
            rpc_url: l1_rpc_args.l1_eth_rpc.clone(),
            slot_duration_override: l1_rpc_args.l1_slot_duration_override,
        };

        // If metrics are enabled, initialize the global cli metrics.
        self.metrics.enabled.then(|| init_rollup_config_metrics(&cfg));

        let jwt_secret = l2_client_args.validate_jwt().await?;

        p2p_flags.check_ports()?;
        let genesis_signer = self.genesis_signer().ok();
        let p2p_config = p2p_flags
            .config(
                &cfg,
                self.l2_chain_id.into(),
                Some(l1_rpc_args.l1_eth_rpc.clone()),
                genesis_signer,
            )
            .await?;
        let rpc_config = rpc_flags.into();

        let engine_config = EngineConfig {
            config: Arc::new(cfg.clone()),
            l2_url: l2_client_args.l2_engine_rpc.clone(),
            l2_jwt_secret: jwt_secret,
            l1_url: l1_rpc_args.l1_eth_rpc.clone(),
            mode: self.node_mode,
        };

        RollupNodeBuilder::new(
            cfg,
            l1_config_builder,
            l2_client_args.l2_trust_rpc,
            engine_config,
            p2p_config,
            rpc_config,
        )
        .with_sequencer_config(sequencer_flags.config())
        .build()
        .start()
        .await
        .map_err(|e| {
            error!(target: "rollup_node", error = %e, "Failed to start rollup node service");
            eyre::eyre!("{e}")
        })?;

        Ok(())
    }
}
