//! P2P CLI Flags

use std::{
    fs,
    net::{IpAddr, SocketAddr, ToSocketAddrs},
    num::{NonZeroUsize, ParseIntError},
    ops::{Deref, DerefMut},
    path::PathBuf,
    str::FromStr,
};

use alloy_primitives::{B256, b256};
use alloy_provider::Provider;
use alloy_signer_local::PrivateKeySigner;
use backon::Retryable;
use base_common_genesis::RollupConfig;
use base_consensus_derive::ChainProvider;
use base_consensus_disc::LocalNode;
use base_consensus_gossip::{
    ConnectionLimitsConfig, DEFAULT_MAX_IDENTIFY_PEERSTORE_PEERS,
    DEFAULT_MAX_PENDING_OUTGOING_CONNECTIONS, DEFAULT_PENDING_DIAL_TIMEOUT, GaterConfig,
};
use base_consensus_node::NetworkConfig;
use base_consensus_peers::{BootNode, BootStoreFile, PeerMonitoring, PeerScoreLevel};
use base_consensus_providers::{AlloyChainProvider, L1RpcProvider};
use base_retry::RetryConfig;
use clap::Parser;
use discv5::enr::k256;
use eyre::{Result, WrapErr};
use libp2p::identity::Keypair;
use tokio::time::Duration;
use tracing::{error, info, warn};
use url::Url;

use crate::signer::{SignerArgs, SignerArgsParseError};

/// Resolves a hostname or IP address string to an [`IpAddr`].
///
/// Accepts either:
/// - A valid IP address string (e.g., "127.0.0.1", "`::1`")
/// - A DNS hostname (e.g., "node1.example.com")
///
/// For DNS hostnames, this performs synchronous DNS resolution and returns the first
/// resolved IP address.
fn resolve_host(host: &str) -> Result<IpAddr, String> {
    // First, try to parse as a direct IP address
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(ip);
    }

    // If that fails, try DNS resolution
    // We append a port to make it a valid socket address for resolution
    let socket_addr = format!("{host}:0");
    match socket_addr.to_socket_addrs() {
        Ok(mut addrs) => addrs
            .next()
            .map(|addr| addr.ip())
            .ok_or_else(|| format!("DNS resolution for '{host}' returned no addresses")),
        Err(e) => Err(format!("Failed to resolve '{host}': {e}")),
    }
}

fn parse_nonzero_usize(arg: &str) -> Result<NonZeroUsize, String> {
    let value = arg.parse::<usize>().map_err(|err| err.to_string())?;
    NonZeroUsize::new(value).ok_or_else(|| "value must be greater than 0".to_string())
}

/// P2P CLI Flags
#[derive(Parser, Clone, Debug, PartialEq, Eq)]
pub struct P2PNetworkArgs {
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
    /// High-tide peer count. The node starts pruning peer connections slowly after reaching this
    /// number.
    #[arg(long = "p2p.peers.hi", default_value = "30", env = "BASE_NODE_P2P_PEERS_HI")]
    pub peers_hi: u32,
    /// Maximum number of outbound libp2p connections that may be pending at once.
    #[arg(
        long = "p2p.max-pending-outgoing",
        default_value_t = DEFAULT_MAX_PENDING_OUTGOING_CONNECTIONS,
        env = "BASE_NODE_P2P_MAX_PENDING_OUTGOING"
    )]
    pub max_pending_outgoing: u32,
    /// Maximum number of peers to retain identify metadata for.
    #[arg(
        long = "p2p.identify.peerstore.size",
        default_value_t = DEFAULT_MAX_IDENTIFY_PEERSTORE_PEERS,
        env = "BASE_NODE_P2P_IDENTIFY_PEERSTORE_SIZE",
        value_parser = parse_nonzero_usize
    )]
    pub identify_peerstore_size: NonZeroUsize,
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
    /// By default, peers are banned if their score is below -100. This follows the reference node default `<https://github.com/ethereum-optimism/optimism/blob/09a8351a72e43647c8a96f98c16bb60e7b25dc6e/op-node/flags/p2p_flags.go#L123-L130>`.
    #[arg(long = "p2p.ban.threshold", default_value = "-100", env = "BASE_NODE_P2P_BAN_THRESHOLD")]
    pub ban_threshold: i64,

    /// The duration in minutes to ban a peer for.
    ///
    /// For peers to be banned, the `p2p.ban.peers` flag must be set to `true`.
    /// By default peers are banned for 1 hour. This follows the reference node default `<https://github.com/ethereum-optimism/optimism/blob/09a8351a72e43647c8a96f98c16bb60e7b25dc6e/op-node/flags/p2p_flags.go#L131-L138>`.
    #[arg(long = "p2p.ban.duration", default_value = "60", env = "BASE_NODE_P2P_BAN_DURATION")]
    pub ban_duration: u64,

    /// The interval in seconds to find peers using the discovery service.
    /// Defaults to 5 seconds.
    #[arg(
        id = "consensus_p2p_discovery_interval",
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

    /// The duration in seconds before a pending outbound dial is aborted.
    #[arg(
        long = "p2p.pending-dial.timeout",
        env = "BASE_NODE_P2P_PENDING_DIAL_TIMEOUT",
        default_value_t = DEFAULT_PENDING_DIAL_TIMEOUT.as_secs()
    )]
    pub pending_dial_timeout: u64,

    /// An optional list of bootnode ENRs or node records to start the node with.
    #[arg(
        id = "consensus_p2p_bootnodes",
        long = "p2p.bootnodes",
        value_delimiter = ',',
        env = "BASE_NODE_P2P_BOOTNODES"
    )]
    pub bootnodes: Vec<String>,

    /// Path to a file containing bootnode ENRs or node records.
    ///
    /// Entries may be separated by newlines or commas.
    #[arg(
        id = "consensus_p2p_bootnodes_file",
        long = "p2p.bootnodes-file",
        env = "BASE_NODE_P2P_BOOTNODES_FILE"
    )]
    pub bootnodes_file: Option<PathBuf>,

    /// Optionally enable topic scoring.
    ///
    /// Topic scoring is a mechanism to score peers based on their behavior in the gossip network.
    /// Historically, topic scoring was only enabled for the v1 topic on the Base p2p network
    /// in the reference node. This was a silent bug, and topic scoring is actively being
    /// [phased out of the reference node][out].
    ///
    /// This flag is only presented for backwards compatibility and debugging purposes.
    ///
    /// [out]: https://github.com/ethereum-optimism/optimism/pull/15719
    #[arg(
        long = "p2p.topic-scoring",
        default_value = "false",
        env = "BASE_NODE_P2P_TOPIC_SCORING"
    )]
    pub topic_scoring: bool,

    /// An optional unsafe block signer address.
    ///
    /// By default, this is fetched from the built-in chain config using the
    /// specified L2 chain ID.
    #[arg(long = "p2p.unsafe.block.signer", env = "BASE_NODE_P2P_UNSAFE_BLOCK_SIGNER")]
    pub unsafe_block_signer: Option<alloy_primitives::Address>,

    /// Maximum number of retry attempts for each L1 JSON-RPC call made while resolving the
    /// unsafe block signer address from L1 during startup.
    ///
    /// Retries use exponential backoff so a transient L1 RPC outage (e.g. a `502` from a
    /// load-balanced RPC proxy) does not crash node startup.
    #[arg(
        long = "p2p.unsafe-block-signer.retry-max-attempts",
        env = "BASE_NODE_P2P_UNSAFE_BLOCK_SIGNER_RETRY_MAX_ATTEMPTS",
        default_value_t = base_retry::DEFAULT_BOUNDED_MAX_ATTEMPTS
    )]
    pub unsafe_block_signer_retry_max_attempts: u32,

    /// Initial backoff delay, in milliseconds, between retries of the L1 JSON-RPC calls used
    /// to resolve the unsafe block signer address from L1.
    #[arg(
        long = "p2p.unsafe-block-signer.retry-initial-delay",
        env = "BASE_NODE_P2P_UNSAFE_BLOCK_SIGNER_RETRY_INITIAL_DELAY",
        default_value_t = base_retry::DEFAULT_BOUNDED_INITIAL_DELAY.as_millis() as u64
    )]
    pub unsafe_block_signer_retry_initial_delay: u64,

    /// Maximum backoff delay, in milliseconds, between retries of the L1 JSON-RPC calls used
    /// to resolve the unsafe block signer address from L1.
    #[arg(
        long = "p2p.unsafe-block-signer.retry-max-delay",
        env = "BASE_NODE_P2P_UNSAFE_BLOCK_SIGNER_RETRY_MAX_DELAY",
        default_value_t = base_retry::DEFAULT_BOUNDED_MAX_DELAY.as_millis() as u64
    )]
    pub unsafe_block_signer_retry_max_delay: u64,

    /// An optional flag to remove random peers from discovery to rotate the peer set.
    ///
    /// This is the number of seconds to wait before removing a peer from the discovery
    /// service. By default, peers are not removed from the discovery service.
    ///
    /// This is useful for discovering a wider set of peers.
    #[arg(long = "p2p.discovery.randomize", env = "BASE_NODE_P2P_DISCOVERY_RANDOMIZE")]
    pub discovery_randomize: Option<u64>,
}

impl Default for P2PNetworkArgs {
    fn default() -> Self {
        // Construct default values using the clap parser.
        // This works since none of the cli flags are required.
        Self::parse_from::<[_; 0], &str>([])
    }
}

/// P2P CLI flags for a node that may sign unsafe block gossip.
#[derive(Parser, Clone, Debug, PartialEq, Eq)]
pub struct P2PArgs {
    /// P2P network configuration.
    #[command(flatten)]
    pub network: P2PNetworkArgs,

    /// Specify optional remote signer configuration. Note that this argument is mutually exclusive
    /// with `p2p.sequencer.key` that specifies a local sequencer signer.
    #[command(flatten)]
    pub signer: SignerArgs,
}

impl Default for P2PArgs {
    fn default() -> Self {
        // Construct default values using the clap parser.
        // This works since none of the cli flags are required.
        Self::parse_from::<[_; 0], &str>([])
    }
}

impl Deref for P2PArgs {
    type Target = P2PNetworkArgs;

    fn deref(&self) -> &Self::Target {
        &self.network
    }
}

impl DerefMut for P2PArgs {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.network
    }
}

/// P2P CLI flags for an embedded validator-only node.
#[derive(Parser, Clone, Debug, PartialEq, Eq)]
pub struct EmbeddedP2PArgs {
    /// P2P network configuration.
    #[command(flatten)]
    pub network: P2PNetworkArgs,
}

impl Default for EmbeddedP2PArgs {
    fn default() -> Self {
        // Construct default values using the clap parser.
        // This works since none of the cli flags are required.
        Self::parse_from::<[_; 0], &str>([])
    }
}

impl From<EmbeddedP2PArgs> for P2PArgs {
    fn from(args: EmbeddedP2PArgs) -> Self {
        Self { network: args.network, signer: SignerArgs::default() }
    }
}

/// Errors that can occur when building a P2P network configuration.
#[derive(Debug, thiserror::Error)]
pub enum P2PConfigError {
    /// Error from signer args parsing.
    #[error(transparent)]
    SignerArgs(#[from] SignerArgsParseError),
    /// Error from eyre.
    #[error(transparent)]
    Eyre(#[from] eyre::Error),
}

impl P2PArgs {
    fn check_ports_inner(ip_addr: IpAddr, tcp_port: u16, udp_port: u16) -> Result<()> {
        if tcp_port == 0 {
            return Ok(());
        }
        if udp_port == 0 {
            return Ok(());
        }
        let tcp_socket = std::net::TcpListener::bind((ip_addr, tcp_port));
        let udp_socket = std::net::UdpSocket::bind((ip_addr, udp_port));
        if let Err(e) = tcp_socket {
            error!(target: "p2p::flags", tcp_port, error = %e, "Error binding TCP socket");
            eyre::bail!("Error binding TCP socket on port {tcp_port}: {e}");
        }
        if let Err(e) = udp_socket {
            error!(target: "p2p::flags", udp_port, error = %e, "Error binding UDP socket");
            eyre::bail!("Error binding UDP socket on port {udp_port}: {e}");
        }

        Ok(())
    }

    /// Checks if the listen ports are available on the system.
    ///
    /// If either of the ports are `0`, this check is skipped.
    ///
    /// ## Errors
    ///
    /// - If the TCP port is already in use.
    /// - If the UDP port is already in use.
    pub fn check_ports(&self) -> Result<()> {
        Self::check_ports_inner(self.listen_ip, self.listen_tcp_port, self.listen_udp_port)
    }

    /// Returns the private key as specified in the raw cli flag or via file path.
    pub fn private_key(&self) -> Option<PrivateKeySigner> {
        if let Some(key) = self.private_key {
            match PrivateKeySigner::from_bytes(&key) {
                Ok(signer) => return Some(signer),
                Err(e) => {
                    error!(target: "p2p::flags", error = %e, "Failed to parse private key");
                    return None;
                }
            }
        }

        if let Some(path) = self.priv_path.as_ref()
            && path.exists()
        {
            let contents = std::fs::read_to_string(path).ok()?;
            let decoded = B256::from_str(&contents).ok()?;
            match PrivateKeySigner::from_bytes(&decoded) {
                Ok(signer) => return Some(signer),
                Err(e) => {
                    error!(target: "p2p::flags", error = %e, "Failed to parse private key from file");
                    return None;
                }
            }
        }

        None
    }

    /// Returns the unsafe block signer from the CLI arguments.
    ///
    /// This method fetches the unsafe block signer from L1 if an RPC URL is provided,
    /// otherwise falls back to the genesis signer or the configured unsafe block signer.
    ///
    /// Each L1 JSON-RPC call made while resolving the signer from L1 is retried independently
    /// with exponential backoff (`--p2p.unsafe-block-signer.retry-*`) so a transient L1 RPC
    /// outage does not crash startup.
    pub async fn unsafe_block_signer(
        &self,
        l2_chain_id: u64,
        rollup_config: &RollupConfig,
        l1_eth_rpc: Option<Url>,
        l1_rpc_timeout: Duration,
        genesis_signer: Option<alloy_primitives::Address>,
    ) -> eyre::Result<alloy_primitives::Address> {
        if let Some(l1_eth_rpc) = l1_eth_rpc {
            /// The storage slot that the unsafe block signer address is stored at.
            /// Computed as: `bytes32(uint256(keccak256("systemconfig.unsafeblocksigner")) - 1)`
            const UNSAFE_BLOCK_SIGNER_ADDRESS_STORAGE_SLOT: B256 =
                b256!("0x65a7ed542fb37fe237fdfbdd70b31598523fe5b32879e307bae27a0bd9581c08");

            let provider = AlloyChainProvider::new(
                L1RpcProvider::new_http_with_timeout(l1_eth_rpc, l1_rpc_timeout),
                1024,
            );

            let retry_config = RetryConfig::new(
                self.unsafe_block_signer_retry_max_attempts,
                Duration::from_millis(self.unsafe_block_signer_retry_initial_delay),
                Duration::from_millis(self.unsafe_block_signer_retry_max_delay),
            );
            // `ExponentialBuilder` is `Copy`; each `.retry(backoff)` below independently
            // starts its own fresh attempt/backoff sequence from this template.
            let backoff = retry_config.to_backoff_builder();

            // Every error here is `RpcError<TransportErrorKind>` (always transport-class) —
            // no `.when()` filter needed, retry unconditionally on `Err`.
            //
            // Each closure below clones the provider/inner handle *inside* the sync closure
            // body, before entering the `async move` block. Cloning `AlloyChainProvider`
            // also clones its LRU caches, but they are empty in this startup-only path. A
            // closure whose returned future instead borrowed `provider` across `.await`
            // could only ever be called once (`FnOnce`), since the borrow would need to
            // outlive individual invocations to satisfy `FnMut`. Giving each attempt's
            // future its own owned clone sidesteps that entirely.
            let latest_block_num = (|| {
                let mut provider = provider.clone();
                async move { provider.latest_block_number().await }
            })
            .retry(backoff)
            .notify(|err, dur| {
                warn!(
                    target: "p2p::flags",
                    error = %err,
                    delay = ?dur,
                    "Retrying L1 latest block number lookup for unsafe block signer resolution"
                );
            })
            .await?;

            // The L1 EL may report a latest block number that it has not fully executed
            // yet (race between header sync and execution). Fall back once to the previous
            // block if the latest block is unavailable — this stays a one-shot decision, as
            // before. Each of the two candidate-block fetches is independently retried;
            // `block_info_by_number` only ever queries `BlockId::Number` here, so both
            // reachable error variants map to `PipelineErrorKind::Temporary` — no `.when()`
            // filter needed.
            let first_attempt = (|| {
                let mut provider = provider.clone();
                async move { provider.block_info_by_number(latest_block_num).await }
            })
            .retry(backoff)
            .notify(|err, dur| {
                warn!(
                    target: "p2p::flags",
                    block_number = latest_block_num,
                    error = %err,
                    delay = ?dur,
                    "Retrying L1 block info lookup for unsafe block signer resolution"
                );
            })
            .await;
            let block_info = match first_attempt {
                Ok(info) => info,
                Err(err) => {
                    warn!(
                        target: "p2p::flags",
                        block_number = latest_block_num,
                        error = %err,
                        "Failed to fetch latest L1 block info after retries, retrying with previous block"
                    );
                    let fallback_block_num = latest_block_num.saturating_sub(1);
                    (|| {
                        let mut provider = provider.clone();
                        async move { provider.block_info_by_number(fallback_block_num).await }
                    })
                    .retry(backoff)
                    .notify(|err, dur| {
                        warn!(
                            target: "p2p::flags",
                            block_number = fallback_block_num,
                            error = %err,
                            delay = ?dur,
                            "Retrying L1 previous-block info lookup for unsafe block signer resolution"
                        );
                    })
                    .await?
                }
            };

            // Fetch the unsafe block signer address from the system config. Raw `Provider`
            // call returning `RpcError<TransportErrorKind>` directly — no `.when()` filter
            // needed.
            let unsafe_block_signer_address = (|| {
                let inner = provider.inner.clone();
                async move {
                    inner
                        .get_storage_at(
                            rollup_config.l1_system_config_address,
                            UNSAFE_BLOCK_SIGNER_ADDRESS_STORAGE_SLOT.into(),
                        )
                        .hash(block_info.hash)
                        .await
                }
            })
            .retry(backoff)
            .notify(|err, dur| {
                warn!(
                    target: "p2p::flags",
                    block_hash = %block_info.hash,
                    error = %err,
                    delay = ?dur,
                    "Retrying L1 SystemConfig storage read for unsafe block signer resolution"
                );
            })
            .await?;

            // Convert the unsafe block signer address to the correct type.
            let signer = alloy_primitives::Address::from_slice(
                &unsafe_block_signer_address.to_be_bytes_vec()[12..],
            );

            // If storage returns zero (e.g. L1 is still early in sync and the SystemConfig
            // contract hadn't been deployed at the queried block), fall through to the
            // genesis/built-in signer rather than using the zero address.
            if !signer.is_zero() {
                return Ok(signer);
            }

            warn!(
                target: "p2p::flags",
                block_number = block_info.number,
                "L1 SystemConfig returned zero unsafe block signer (L1 may still be syncing), \
                 falling back to built-in/genesis signer"
            );
        }

        // Otherwise use the genesis signer or the configured unsafe block signer.
        genesis_signer.or(self.unsafe_block_signer).ok_or_else(|| {
            eyre::eyre!(
                "Unsafe block signer not provided for chain ID {}. \
                 Provide --p2p.unsafe.block.signer or ensure the chain is supported by the built-in chain config.",
                l2_chain_id
            )
        })
    }

    /// Constructs the P2P network [`NetworkConfig`] from CLI arguments.
    ///
    /// ## Parameters
    ///
    /// - `config`: The rollup configuration.
    /// - `l2_chain_id`: The L2 chain ID.
    /// - `l1_rpc`: Optional L1 RPC URL for fetching the unsafe block signer.
    /// - `l1_rpc_timeout`: Request timeout for calls made while fetching the unsafe block signer.
    /// - `genesis_signer`: Optional genesis signer address.
    ///
    /// Errors if the genesis unsafe block signer isn't available for the specified L2 Chain ID.
    pub async fn config(
        self,
        config: &RollupConfig,
        l2_chain_id: u64,
        l1_rpc: Option<Url>,
        l1_rpc_timeout: Duration,
        genesis_signer: Option<alloy_primitives::Address>,
    ) -> Result<NetworkConfig, P2PConfigError> {
        // Note: the advertised address is contained in the ENR for external peers from the
        // discovery layer to use.

        // Fallback to the listen ip if the advertise ip is not specified
        let advertise_ip = self.advertise_ip.unwrap_or(self.listen_ip);

        // If the advertise ip is set, we will disable the dynamic ENR updates.
        let static_ip = self.advertise_ip.is_some();

        // If an advertised port is unset or zero, use the corresponding listen port.
        let advertise_tcp_port = match self.advertise_tcp_port {
            None | Some(0) => self.listen_tcp_port,
            Some(port) => port,
        };

        let advertise_udp_port = match self.advertise_udp_port {
            None | Some(0) => self.listen_udp_port,
            Some(port) => port,
        };

        let keypair = self.keypair().unwrap_or_else(|e| {
            let generated = Keypair::generate_secp256k1();
            warn!(
                target: "p2p::config",
                error = %e,
                peer_id = %generated.public().to_peer_id(),
                "Failed to load P2P keypair from configuration, generated ephemeral keypair. \
                 Set --p2p.priv.path or --p2p.priv.raw for a persistent peer ID."
            );
            generated
        });
        let secp256k1_key = keypair.clone().try_into_secp256k1()
            .map_err(|e| eyre::eyre!("Impossible to convert keypair to secp256k1. This is a bug since we only support secp256k1 keys: {e}"))?
            .secret().to_bytes();
        let local_node_key = k256::ecdsa::SigningKey::from_bytes(&secp256k1_key.into())
            .map_err(|e| eyre::eyre!("Impossible to convert keypair to k256 signing key. This is a bug since we only support secp256k1 keys: {e}"))?;

        let discovery_address =
            LocalNode::new(local_node_key, advertise_ip, advertise_tcp_port, advertise_udp_port);
        let gossip_config = base_consensus_gossip::default_config_builder()
            .mesh_n(self.gossip_mesh_d)
            .mesh_n_low(self.gossip_mesh_dlo)
            .mesh_n_high(self.gossip_mesh_dhi)
            .gossip_lazy(self.gossip_mesh_dlazy)
            .flood_publish(self.gossip_flood_publish)
            .build()
            .map_err(|e| eyre::eyre!("Failed to build gossip config: {e}"))?;

        let monitor_peers = self.ban_enabled.then_some(PeerMonitoring {
            ban_duration: Duration::from_secs(60 * self.ban_duration),
            ban_threshold: self.ban_threshold as f64,
        });

        let discovery_listening_address = SocketAddr::new(self.listen_ip, self.listen_udp_port);
        let discovery_config =
            NetworkConfig::discv5_config(discovery_listening_address.into(), static_ip);

        let mut gossip_address = libp2p::Multiaddr::from(self.listen_ip);
        gossip_address.push(libp2p::multiaddr::Protocol::Tcp(self.listen_tcp_port));

        let unsafe_block_signer = self
            .unsafe_block_signer(l2_chain_id, config, l1_rpc, l1_rpc_timeout, genesis_signer)
            .await?;

        let bootnodes = self
            .bootnode_strings()?
            .into_iter()
            .map(|bootnode| {
                BootNode::parse_bootnode(&bootnode)
                    .map_err(|e| eyre::eyre!("Failed to parse bootnode '{bootnode}': {e}"))
            })
            .collect::<Result<Vec<BootNode>>>()?
            .into();

        let bootstore =
            if self.disable_bootstore {
                None
            } else {
                Some(self.bootstore.clone().map_or(
                    BootStoreFile::Default { chain_id: l2_chain_id },
                    BootStoreFile::Custom,
                ))
            };

        Ok(NetworkConfig {
            discovery_config,
            discovery_interval: Duration::from_secs(self.discovery_interval),
            discovery_address,
            discovery_randomize: self.discovery_randomize.map(Duration::from_secs),
            enr_update: !static_ip,
            gossip_address,
            keypair,
            unsafe_block_signer,
            gossip_config,
            scoring: self.scoring,
            monitor_peers,
            bootstore,
            topic_scoring: self.topic_scoring,
            gater_config: GaterConfig {
                peer_redialing: self.peer_redial,
                dial_period: Duration::from_secs(60 * self.redial_period),
                pending_dial_timeout: Duration::from_secs(self.pending_dial_timeout),
            },
            connection_limits_config: ConnectionLimitsConfig {
                max_pending_outgoing: self.max_pending_outgoing,
                ..ConnectionLimitsConfig::new(self.peers_hi)
            },
            max_identify_peerstore_peers: self.identify_peerstore_size,
            bootnodes,
            rollup_config: config.clone(),
            gossip_signer: self.signer.config(l2_chain_id)?,
        })
    }

    /// Returns the [`Keypair`] from the cli inputs.
    ///
    /// If the raw private key is empty and the specified file is empty,
    /// this method will generate a new private key and write it out to the file.
    ///
    /// If neither a file is specified, nor a raw private key input, this method
    /// will error.
    pub fn keypair(&self) -> Result<Keypair> {
        // Attempt the parse the private key if specified.
        if let Some(mut private_key) = self.private_key {
            let keypair = base_consensus_peers::SecretKeyLoader::parse(&mut private_key.0)
                .map_err(|e| eyre::eyre!(e))?;
            info!(
                target: "p2p::config",
                peer_id = %keypair.public().to_peer_id(),
                "Successfully loaded P2P keypair from raw private key"
            );
            return Ok(keypair);
        }

        let Some(ref key_path) = self.priv_path else {
            eyre::bail!("Neither a raw private key nor a private key file path was provided.");
        };

        base_consensus_peers::SecretKeyLoader::load(key_path).map_err(|e| eyre::eyre!(e))
    }

    fn bootnode_strings(&self) -> Result<Vec<String>> {
        let mut bootnodes = self.bootnodes.clone();

        if let Some(path) = &self.bootnodes_file {
            let contents = fs::read_to_string(path)
                .wrap_err_with(|| format!("Failed to read bootnodes file {}", path.display()))?;
            bootnodes.extend(
                contents
                    .split([',', '\n'])
                    .map(str::trim)
                    .filter(|bootnode| !bootnode.is_empty())
                    .map(ToOwned::to_owned),
            );
        }

        Ok(bootnodes)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use alloy_primitives::{Address, b256};
    use base_common_genesis::RollupConfig;
    use base_consensus_peers::NodeRecord;
    use base_consensus_providers::L1_RPC_TIMEOUT;
    use clap::Parser;
    use httpmock::{HttpMockRequest, HttpMockResponse, Method::POST, MockServer};
    use rstest::rstest;
    use serde_json::{Value, json};

    use super::*;

    /// A mock command that uses the `P2PArgs`.
    #[derive(Parser, Debug, Clone)]
    #[command(about = "Mock command")]
    struct MockCommand {
        /// P2P CLI Flags
        #[clap(flatten)]
        pub p2p: P2PArgs,
    }

    #[test]
    fn test_p2p_args_keypair_missing_both() {
        let args = MockCommand::parse_from(["test"]);
        assert!(args.p2p.keypair().is_err());
    }

    #[test]
    fn test_p2p_args_keypair_raw_private_key() {
        let args = MockCommand::parse_from([
            "test",
            "--p2p.priv.raw",
            "1d2b0bda21d56b8bd12d4f94ebacffdfb35f5e226f84b461103bb8beab6353be",
        ]);
        assert!(args.p2p.keypair().is_ok());
    }

    #[test]
    fn test_p2p_args_keypair_from_path() {
        // Create a temporary directory.
        let dir = std::env::temp_dir();
        let mut source_path = dir.clone();
        assert!(std::env::set_current_dir(dir).is_ok());

        // Write a private key to a file.
        let key = b256!("1d2b0bda21d56b8bd12d4f94ebacffdfb35f5e226f84b461103bb8beab6353be");
        let hex = alloy_primitives::hex::encode(key.0);
        source_path.push("test.txt");
        std::fs::write(&source_path, &hex).unwrap();

        // Parse the keypair from the file.
        let args =
            MockCommand::parse_from(["test", "--p2p.priv.path", source_path.to_str().unwrap()]);
        assert!(args.p2p.keypair().is_ok());
    }

    #[test]
    fn test_p2p_args() {
        let args = MockCommand::parse_from(["test"]);
        assert_eq!(args.p2p, P2PArgs::default());
    }

    #[test]
    fn test_p2p_args_randomized() {
        let args = MockCommand::parse_from(["test", "--p2p.discovery.randomize", "10"]);
        assert_eq!(args.p2p.discovery_randomize, Some(10));
        let args = MockCommand::parse_from(["test"]);
        assert_eq!(args.p2p.discovery_randomize, None);
    }

    #[test]
    fn test_p2p_args_no_discovery() {
        let args = MockCommand::parse_from(["test", "--p2p.no-discovery"]);
        assert!(args.p2p.no_discovery);
    }

    #[test]
    fn test_p2p_args_priv_path() {
        let args = MockCommand::parse_from(["test", "--p2p.priv.path", "test.txt"]);
        assert_eq!(args.p2p.priv_path, Some(PathBuf::from("test.txt")));
    }

    #[test]
    fn test_p2p_args_private_key() {
        let args = MockCommand::parse_from([
            "test",
            "--p2p.priv.raw",
            "1d2b0bda21d56b8bd12d4f94ebacffdfb35f5e226f84b461103bb8beab6353be",
        ]);
        let key = b256!("1d2b0bda21d56b8bd12d4f94ebacffdfb35f5e226f84b461103bb8beab6353be");
        assert_eq!(args.p2p.private_key, Some(key));
    }

    #[test]
    fn test_p2p_args_sequencer_key() {
        let args = MockCommand::parse_from([
            "test",
            "--p2p.sequencer.key",
            "bcc617ea05150ff60490d3c6058630ba94ae9f12a02a87efd291349ca0e54e0a",
        ]);
        let key = b256!("bcc617ea05150ff60490d3c6058630ba94ae9f12a02a87efd291349ca0e54e0a");
        assert_eq!(args.p2p.signer.sequencer_key, Some(key));
    }

    #[test]
    fn test_p2p_args_listen_ip() {
        let args = MockCommand::parse_from(["test", "--p2p.listen.ip", "127.0.0.1"]);
        let expected: IpAddr = "127.0.0.1".parse().unwrap();
        assert_eq!(args.p2p.listen_ip, expected);
    }

    #[test]
    fn test_p2p_args_listen_tcp_port() {
        let args = MockCommand::parse_from(["test", "--p2p.listen.tcp", "1234"]);
        assert_eq!(args.p2p.listen_tcp_port, 1234);
    }

    #[test]
    fn test_p2p_args_listen_udp_port() {
        let args = MockCommand::parse_from(["test", "--p2p.listen.udp", "1234"]);
        assert_eq!(args.p2p.listen_udp_port, 1234);
    }

    #[test]
    fn test_p2p_args_bootnodes() {
        let args = MockCommand::parse_from([
            "test",
            "--p2p.bootnodes",
            "enode://ca2774c3c401325850b2477fd7d0f27911efbf79b1e8b335066516e2bd8c4c9e0ba9696a94b1cb030a88eac582305ff55e905e64fb77fe0edcd70a4e5296d3ec@34.65.175.185:30305",
        ]);
        assert_eq!(
            args.p2p.bootnodes,
            vec![
                "enode://ca2774c3c401325850b2477fd7d0f27911efbf79b1e8b335066516e2bd8c4c9e0ba9696a94b1cb030a88eac582305ff55e905e64fb77fe0edcd70a4e5296d3ec@34.65.175.185:30305",
            ]
        );

        // Parse the bootnodes.
        let bootnodes = args
            .p2p
            .bootnodes
            .iter()
            .map(|bootnode| BootNode::parse_bootnode(bootnode))
            .collect::<std::result::Result<Vec<BootNode>, _>>()
            .expect("test bootnode should parse");

        // Otherwise, attempt to use the Node Record format.
        let record = NodeRecord::from_str(
                "enode://ca2774c3c401325850b2477fd7d0f27911efbf79b1e8b335066516e2bd8c4c9e0ba9696a94b1cb030a88eac582305ff55e905e64fb77fe0edcd70a4e5296d3ec@34.65.175.185:30305").unwrap();
        let expected_bootnode = vec![BootNode::from_unsigned(record).unwrap()];

        assert_eq!(bootnodes, expected_bootnode);
    }

    #[test]
    fn test_p2p_args_bootnodes_multiple() {
        let args = MockCommand::parse_from([
            "test",
            "--p2p.bootnodes",
            "enode://ca2774c3c401325850b2477fd7d0f27911efbf79b1e8b335066516e2bd8c4c9e0ba9696a94b1cb030a88eac582305ff55e905e64fb77fe0edcd70a4e5296d3ec@34.65.175.185:30305,enode://dd751a9ef8912be1bfa7a5e34e2c3785cc5253110bd929f385e07ba7ac19929fb0e0c5d93f77827291f4da02b2232240fbc47ea7ce04c46e333e452f8656b667@34.65.107.0:30305",
        ]);
        assert_eq!(
            args.p2p.bootnodes,
            vec![
                "enode://ca2774c3c401325850b2477fd7d0f27911efbf79b1e8b335066516e2bd8c4c9e0ba9696a94b1cb030a88eac582305ff55e905e64fb77fe0edcd70a4e5296d3ec@34.65.175.185:30305",
                "enode://dd751a9ef8912be1bfa7a5e34e2c3785cc5253110bd929f385e07ba7ac19929fb0e0c5d93f77827291f4da02b2232240fbc47ea7ce04c46e333e452f8656b667@34.65.107.0:30305",
            ]
        );
    }

    #[test]
    fn test_p2p_args_bootnode_enr() {
        let args = MockCommand::parse_from([
            "test",
            "--p2p.bootnodes",
            "enr:-J64QBbwPjPLZ6IOOToOLsSjtFUjjzN66qmBZdUexpO32Klrc458Q24kbty2PdRaLacHM5z-cZQr8mjeQu3pik6jPSOGAYYFIqBfgmlkgnY0gmlwhDaRWFWHb3BzdGFja4SzlAUAiXNlY3AyNTZrMaECmeSnJh7zjKrDSPoNMGXoopeDF4hhpj5I0OsQUUt4u8uDdGNwgiQGg3VkcIIkBg",
        ]);
        assert_eq!(
            args.p2p.bootnodes,
            vec![
                "enr:-J64QBbwPjPLZ6IOOToOLsSjtFUjjzN66qmBZdUexpO32Klrc458Q24kbty2PdRaLacHM5z-cZQr8mjeQu3pik6jPSOGAYYFIqBfgmlkgnY0gmlwhDaRWFWHb3BzdGFja4SzlAUAiXNlY3AyNTZrMaECmeSnJh7zjKrDSPoNMGXoopeDF4hhpj5I0OsQUUt4u8uDdGNwgiQGg3VkcIIkBg",
            ]
        );
    }

    #[test]
    fn test_p2p_args_bootnodes_file() {
        let args = MockCommand::parse_from(["test", "--p2p.bootnodes-file", "/tmp/bootnodes.txt"]);

        assert_eq!(args.p2p.bootnodes_file, Some(PathBuf::from("/tmp/bootnodes.txt")));
    }

    #[test]
    fn test_p2p_bootnode_strings_reads_file_entries() {
        let file = tempfile::NamedTempFile::new().expect("bootnode file should be created");
        std::fs::write(
            file.path(),
            "enode://ca2774c3c401325850b2477fd7d0f27911efbf79b1e8b335066516e2bd8c4c9e0ba9696a94b1cb030a88eac582305ff55e905e64fb77fe0edcd70a4e5296d3ec@34.65.175.185:30305,\n\
             enode://dd751a9ef8912be1bfa7a5e34e2c3785cc5253110bd929f385e07ba7ac19929fb0e0c5d93f77827291f4da02b2232240fbc47ea7ce04c46e333e452f8656b667@34.65.107.0:30305\n",
        )
        .expect("bootnode file should be written");
        let args = MockCommand::parse_from([
            "test",
            "--p2p.bootnodes",
            "enode://2bd2e657bb3c8efffb8ff6db9071d9eb7be70d7c6d7d980ff80fc93b2629675c5f750bc0a5ef27cd788c2e491b8795a7e9a4a6e72178c14acc6753c0e5d77ae4@34.65.205.244:30305",
            "--p2p.bootnodes-file",
            file.path().to_str().expect("temp path should be utf8"),
        ]);

        let bootnodes = args.p2p.bootnode_strings().expect("bootnodes should load");

        assert_eq!(bootnodes.len(), 3);
        assert!(bootnodes.iter().all(|bootnode| bootnode.starts_with("enode://")));
    }

    #[tokio::test]
    async fn test_unsafe_block_signer_uses_genesis_when_no_l1() {
        let args = MockCommand::parse_from(["test"]).p2p;
        let genesis: Address = "0xAf6E19BE0F9cE7f8afd49a1824851023A8249e8a".parse().unwrap();
        let signer = args
            .unsafe_block_signer(
                8453,
                &RollupConfig::default(),
                None,
                L1_RPC_TIMEOUT,
                Some(genesis),
            )
            .await
            .unwrap();
        assert_eq!(signer, genesis);
    }

    #[tokio::test]
    async fn test_unsafe_block_signer_uses_cli_flag_when_no_genesis() {
        let expected: Address = "0xAf6E19BE0F9cE7f8afd49a1824851023A8249e8a".parse().unwrap();
        let args = MockCommand::parse_from([
            "test",
            "--p2p.unsafe.block.signer",
            "0xAf6E19BE0F9cE7f8afd49a1824851023A8249e8a",
        ])
        .p2p;
        let signer = args
            .unsafe_block_signer(8453, &RollupConfig::default(), None, L1_RPC_TIMEOUT, None)
            .await
            .unwrap();
        assert_eq!(signer, expected);
    }

    #[tokio::test]
    async fn test_unsafe_block_signer_genesis_takes_priority_over_cli() {
        let genesis: Address = "0xAf6E19BE0F9cE7f8afd49a1824851023A8249e8a".parse().unwrap();
        let args = MockCommand::parse_from([
            "test",
            "--p2p.unsafe.block.signer",
            "0x0000000000000000000000000000000000000001",
        ])
        .p2p;
        let signer = args
            .unsafe_block_signer(
                8453,
                &RollupConfig::default(),
                None,
                L1_RPC_TIMEOUT,
                Some(genesis),
            )
            .await
            .unwrap();
        assert_eq!(signer, genesis);
    }

    #[tokio::test]
    async fn test_unsafe_block_signer_errors_with_no_fallbacks() {
        let args = MockCommand::parse_from(["test"]).p2p;
        let err = args
            .unsafe_block_signer(99999, &RollupConfig::default(), None, L1_RPC_TIMEOUT, None)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("99999"));
    }

    #[test]
    fn test_p2p_args_unsafe_block_signer_retry_defaults() {
        let args = MockCommand::parse_from(["test"]).p2p;
        assert_eq!(
            args.unsafe_block_signer_retry_max_attempts,
            base_retry::DEFAULT_BOUNDED_MAX_ATTEMPTS
        );
        assert_eq!(
            args.unsafe_block_signer_retry_initial_delay,
            base_retry::DEFAULT_BOUNDED_INITIAL_DELAY.as_millis() as u64
        );
        assert_eq!(
            args.unsafe_block_signer_retry_max_delay,
            base_retry::DEFAULT_BOUNDED_MAX_DELAY.as_millis() as u64
        );
    }

    #[test]
    fn test_p2p_args_unsafe_block_signer_retry_flags_override() {
        let args = MockCommand::parse_from([
            "test",
            "--p2p.unsafe-block-signer.retry-max-attempts",
            "7",
            "--p2p.unsafe-block-signer.retry-initial-delay",
            "50",
            "--p2p.unsafe-block-signer.retry-max-delay",
            "2000",
        ])
        .p2p;
        assert_eq!(args.unsafe_block_signer_retry_max_attempts, 7);
        assert_eq!(args.unsafe_block_signer_retry_initial_delay, 50);
        assert_eq!(args.unsafe_block_signer_retry_max_delay, 2000);
    }

    fn args_with_retry(max_attempts: u32, delay_ms: u64) -> P2PArgs {
        MockCommand::parse_from([
            "test",
            "--p2p.unsafe-block-signer.retry-max-attempts",
            &max_attempts.to_string(),
            "--p2p.unsafe-block-signer.retry-initial-delay",
            &delay_ms.to_string(),
            "--p2p.unsafe-block-signer.retry-max-delay",
            &delay_ms.to_string(),
        ])
        .p2p
    }

    fn json_rpc_response(req: &HttpMockRequest, result: Value) -> String {
        let id = serde_json::from_slice::<Value>(&req.body_vec())
            .ok()
            .and_then(|body| body.get("id").cloned())
            .unwrap_or(Value::Null);
        json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string()
    }

    fn block_json(number: u64) -> Value {
        json!({
            "hash": "0x1111111111111111111111111111111111111111111111111111111111111111",
            "parentHash": "0x2222222222222222222222222222222222222222222222222222222222222222",
            "sha3Uncles": "0x1dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0a142fd40d49347",
            "miner": "0x0000000000000000000000000000000000000000",
            "stateRoot": "0x3333333333333333333333333333333333333333333333333333333333333333",
            "transactionsRoot": "0x56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421",
            "receiptsRoot": "0x56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421",
            "logsBloom": format!("0x{}", "00".repeat(256)),
            "difficulty": "0x0",
            "number": format!("0x{number:x}"),
            "gasLimit": "0x1c9c380",
            "gasUsed": "0x0",
            "timestamp": "0x1",
            "extraData": "0x",
            "mixHash": "0x0000000000000000000000000000000000000000000000000000000000000000",
            "nonce": "0x0000000000000000",
            "baseFeePerGas": "0x1",
            "transactions": [],
            "uncles": [],
            "withdrawals": [],
            "blobGasUsed": "0x0",
            "excessBlobGas": "0x0"
        })
    }

    /// Mocks a transient-then-success sequence for a single JSON-RPC method: the first hit
    /// returns a `502`, every hit after that returns `success_result`.
    async fn mock_transient_then_success(
        server: &MockServer,
        method: &'static str,
        success_result: Value,
    ) {
        let hits = Arc::new(AtomicUsize::new(0));
        server
            .mock_async(move |when, then| {
                when.method(POST)
                    .path("/")
                    .json_body_includes(format!(r#"{{"method":"{method}"}}"#));
                then.respond_with(move |req| {
                    if hits.fetch_add(1, Ordering::SeqCst) == 0 {
                        HttpMockResponse::builder().status(502).build()
                    } else {
                        HttpMockResponse::builder()
                            .status(200)
                            .header("content-type", "application/json")
                            .body(json_rpc_response(req, success_result.clone()))
                            .build()
                    }
                });
            })
            .await;
    }

    #[tokio::test]
    async fn test_unsafe_block_signer_retries_transient_l1_errors_then_succeeds() {
        let server = MockServer::start_async().await;
        let block_number = 42u64;
        let expected_signer: Address =
            "0x0000000000000000000000000000000000001234".parse().unwrap();
        let storage_value = format!(
            "0x{}{}",
            "00".repeat(12),
            alloy_primitives::hex::encode(expected_signer.as_slice())
        );

        mock_transient_then_success(
            &server,
            "eth_blockNumber",
            json!(format!("0x{block_number:x}")),
        )
        .await;
        mock_transient_then_success(&server, "eth_getBlockByNumber", block_json(block_number))
            .await;
        mock_transient_then_success(&server, "eth_getStorageAt", json!(storage_value)).await;

        let args = args_with_retry(3, 1);
        let l1_rpc = server.url("/").parse().unwrap();
        let signer = args
            .unsafe_block_signer(8453, &RollupConfig::default(), Some(l1_rpc), L1_RPC_TIMEOUT, None)
            .await
            .unwrap();

        assert_eq!(signer, expected_signer);
    }

    #[tokio::test]
    async fn test_unsafe_block_signer_exhausts_retries_on_persistent_l1_outage() {
        let server = MockServer::start_async().await;
        let mock = server
            .mock_async(|when, then| {
                when.method(POST).path("/").json_body_includes(r#"{"method":"eth_blockNumber"}"#);
                then.status(502);
            })
            .await;

        // `max_attempts = 1` retry after the initial call => 2 total calls before giving up.
        let args = args_with_retry(1, 1);
        let l1_rpc = server.url("/").parse().unwrap();
        let err = args
            .unsafe_block_signer(8453, &RollupConfig::default(), Some(l1_rpc), L1_RPC_TIMEOUT, None)
            .await
            .unwrap_err();

        let err = err.to_string();
        assert!(err.contains("502"), "unexpected error: {err}");
        mock.assert_calls_async(2).await;
    }

    #[rstest]
    #[case::omitted(None, None, 9222, 9223)]
    #[case::zero(Some("0"), Some("0"), 9222, 9223)]
    #[case::overrides(Some("9333"), Some("9444"), 9333, 9444)]
    #[case::zero_tcp(Some("0"), Some("9444"), 9222, 9444)]
    #[case::zero_udp(Some("9333"), Some("0"), 9333, 9223)]
    #[tokio::test]
    async fn test_p2p_config_advertised_ports(
        #[case] tcp: Option<&str>,
        #[case] udp: Option<&str>,
        #[case] expected_tcp: u16,
        #[case] expected_udp: u16,
    ) {
        let mut cli_args = vec![
            "test",
            "--p2p.listen.tcp",
            "9222",
            "--p2p.listen.udp",
            "9223",
            "--p2p.advertise.ip",
            "192.0.2.1",
        ];
        if let Some(port) = tcp {
            cli_args.extend(["--p2p.advertise.tcp", port]);
        }
        if let Some(port) = udp {
            cli_args.extend(["--p2p.advertise.udp", port]);
        }
        let args = MockCommand::parse_from(cli_args);
        let config = args
            .p2p
            .config(&RollupConfig::default(), 8453, None, L1_RPC_TIMEOUT, Some(Address::ZERO))
            .await
            .unwrap();
        let enr = config.discovery_address.build_enr(8453).unwrap();

        assert_eq!(enr.tcp4(), Some(expected_tcp));
        assert_eq!(enr.udp4(), Some(expected_udp));
    }

    #[tokio::test]
    async fn test_p2p_config_errors_on_invalid_bootnode() {
        let args = MockCommand::parse_from(["test", "--p2p.bootnodes", "enr:invalid"]);

        let err = args
            .p2p
            .config(&RollupConfig::default(), 8453, None, L1_RPC_TIMEOUT, Some(Address::ZERO))
            .await
            .expect_err("invalid bootnode should fail config")
            .to_string();

        assert!(err.contains("Failed to parse bootnode 'enr:invalid'"));
    }

    #[tokio::test]
    async fn test_p2p_config_wires_peer_high_tide_to_connection_limits() {
        let args = MockCommand::parse_from(["test", "--p2p.peers.hi", "42"]);

        let config = args
            .p2p
            .config(&RollupConfig::default(), 8453, None, L1_RPC_TIMEOUT, Some(Address::ZERO))
            .await
            .unwrap();

        assert_eq!(config.connection_limits_config.max_established_incoming, 42);
        assert_eq!(config.connection_limits_config.max_established_outgoing, 42);
        assert_eq!(config.connection_limits_config.max_established, 42);
    }

    #[tokio::test]
    async fn test_p2p_config_wires_max_pending_outgoing_to_connection_limits() {
        let args = MockCommand::parse_from(["test", "--p2p.max-pending-outgoing", "64"]);

        let config = args
            .p2p
            .config(&RollupConfig::default(), 8453, None, L1_RPC_TIMEOUT, Some(Address::ZERO))
            .await
            .unwrap();

        assert_eq!(config.connection_limits_config.max_pending_outgoing, 64);
    }

    #[tokio::test]
    async fn test_p2p_config_wires_identify_peerstore_size() {
        let args = MockCommand::parse_from(["test", "--p2p.identify.peerstore.size", "2048"]);

        let config = args
            .p2p
            .config(&RollupConfig::default(), 8453, None, L1_RPC_TIMEOUT, Some(Address::ZERO))
            .await
            .unwrap();

        assert_eq!(config.max_identify_peerstore_peers.get(), 2048);
    }

    #[tokio::test]
    async fn test_p2p_config_wires_pending_dial_timeout() {
        let args = MockCommand::parse_from(["test", "--p2p.pending-dial.timeout", "45"]);

        let config = args
            .p2p
            .config(&RollupConfig::default(), 8453, None, L1_RPC_TIMEOUT, Some(Address::ZERO))
            .await
            .unwrap();

        assert_eq!(config.gater_config.pending_dial_timeout, Duration::from_secs(45));
    }

    #[test]
    fn test_p2p_args_reject_zero_identify_peerstore_size() {
        let err = MockCommand::try_parse_from(["test", "--p2p.identify.peerstore.size", "0"])
            .expect_err("zero identify peerstore size should fail")
            .to_string();

        assert!(err.contains("value must be greater than 0"));
    }

    #[test]
    fn test_p2p_args_listen_ip_dns_resolution() {
        // Test that DNS hostnames are resolved to IP addresses
        // Using localhost which should resolve reliably
        let args = MockCommand::parse_from(["test", "--p2p.listen.ip", "localhost"]);
        // localhost typically resolves to 127.0.0.1 or ::1
        assert!(
            args.p2p.listen_ip == "127.0.0.1".parse::<IpAddr>().unwrap()
                || args.p2p.listen_ip == "::1".parse::<IpAddr>().unwrap()
        );
    }

    #[test]
    fn test_p2p_args_advertise_ip_dns_resolution() {
        // Test that DNS hostnames are resolved to IP addresses for advertise_ip
        let args = MockCommand::parse_from(["test", "--p2p.advertise.ip", "localhost"]);
        // localhost typically resolves to 127.0.0.1 or ::1
        let ip = args.p2p.advertise_ip.unwrap();
        assert!(
            ip == "127.0.0.1".parse::<IpAddr>().unwrap() || ip == "::1".parse::<IpAddr>().unwrap()
        );
    }

    #[test]
    fn test_resolve_host_with_ip() {
        // Test that IP addresses are passed through directly
        let ip = resolve_host("192.168.1.1").unwrap();
        assert_eq!(ip, "192.168.1.1".parse::<IpAddr>().unwrap());

        let ipv6 = resolve_host("::1").unwrap();
        assert_eq!(ipv6, "::1".parse::<IpAddr>().unwrap());
    }

    #[test]
    fn test_resolve_host_with_dns() {
        // Test DNS resolution with localhost
        let ip = resolve_host("localhost").unwrap();
        assert!(
            ip == "127.0.0.1".parse::<IpAddr>().unwrap() || ip == "::1".parse::<IpAddr>().unwrap()
        );
    }

    #[test]
    fn test_resolve_host_invalid() {
        // Test that invalid hostnames return an error
        let result = resolve_host("this-hostname-definitely-does-not-exist.invalid");
        assert!(result.is_err());
    }
}
