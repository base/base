//! P2P CLI Flags
//!
//! These are based on p2p flags from the [`op-node`][op-node] CLI.
//!
//! [op-node]: https://github.com/ethereum-optimism/optimism/blob/develop/op-node/flags/p2p_flags.go

use std::{
    net::{IpAddr, SocketAddr, ToSocketAddrs},
    path::PathBuf,
    str::FromStr,
};

use alloy_primitives::{B256, b256};
use alloy_provider::Provider;
use alloy_signer_local::PrivateKeySigner;
use base_consensus_derive::ChainProvider;
use base_consensus_disc::LocalNode;
use base_consensus_genesis::RollupConfig;
use base_consensus_gossip::GaterConfig;
use base_consensus_node::NetworkConfig;
use base_consensus_peers::{BootNode, BootStoreFile, PeerMonitoring, PeerScoreLevel};
use base_consensus_providers::AlloyChainProvider;
use discv5::enr::k256;
use eyre::Result;
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
pub fn resolve_host(host: &str) -> Result<IpAddr, String> {
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

/// P2P CLI Flags
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct P2PArgs {
    /// Disable Discv5 (node discovery).
    pub no_discovery: bool,
    /// Read the hex-encoded 32-byte private key for the peer ID from this txt file.
    /// Created if not already exists. Important to persist to keep the same network identity after
    /// restarting, maintaining the previous advertised identity.
    pub priv_path: Option<PathBuf>,
    /// The hex-encoded 32-byte private key for the peer ID.
    pub private_key: Option<B256>,

    /// IP address or DNS hostname to advertise to external peers from Discv5.
    /// Optional argument. Use the `p2p.listen.ip` if not set.
    /// Accepts either an IP address (e.g., "1.2.3.4") or a DNS hostname (e.g.,
    /// "node1.example.com"). DNS hostnames are resolved to IP addresses at startup.
    ///
    /// Technical note: if this argument is set, the dynamic ENR updates from the discovery layer
    /// will be disabled. This is to allow the advertised IP to be static (to use in a network
    /// behind a NAT for instance).
    pub advertise_ip: Option<IpAddr>,
    /// TCP port to advertise to external peers from the discovery layer. Same as `p2p.listen.tcp`
    /// if set to zero.
    pub advertise_tcp_port: Option<u16>,
    /// UDP port to advertise to external peers from the discovery layer.
    /// Same as `p2p.listen.udp` if set to zero.
    pub advertise_udp_port: Option<u16>,

    /// IP address or DNS hostname to bind LibP2P/Discv5 to.
    /// Accepts either an IP address (e.g., "0.0.0.0") or a DNS hostname (e.g.,
    /// "node1.example.com"). DNS hostnames are resolved to IP addresses at startup.
    pub listen_ip: IpAddr,
    /// TCP port to bind `LibP2P` to. Any available system port if set to 0.
    pub listen_tcp_port: u16,
    /// UDP port to bind Discv5 to. Same as TCP port if left 0.
    pub listen_udp_port: u16,
    /// Low-tide peer count. The node actively searches for new peer connections if below this
    /// amount.
    pub peers_lo: u32,
    /// High-tide peer count. The node starts pruning peer connections slowly after reaching this
    /// number.
    pub peers_hi: u32,
    /// Grace period to keep a newly connected peer around, if it is not misbehaving.
    pub peers_grace: Duration,
    /// Configure `GossipSub` topic stable mesh target count.
    /// Aka: The desired outbound degree (numbers of peers to gossip to).
    pub gossip_mesh_d: usize,
    /// Configure `GossipSub` topic stable mesh low watermark.
    /// Aka: The lower bound of outbound degree.
    pub gossip_mesh_dlo: usize,
    /// Configure `GossipSub` topic stable mesh high watermark.
    /// Aka: The upper bound of outbound degree (additional peers will not receive gossip).
    pub gossip_mesh_dhi: usize,
    /// Configure `GossipSub` gossip target.
    /// Aka: The target degree for gossip only (not messaging like p2p.gossip.mesh.d, just
    /// announcements of IHAVE).
    pub gossip_mesh_dlazy: usize,
    /// Configure `GossipSub` to publish messages to all known peers on the topic, outside of the
    /// mesh. Also see Dlazy as less aggressive alternative.
    pub gossip_flood_publish: bool,
    /// Sets the peer scoring strategy for the P2P stack.
    /// Can be one of: none or light.
    pub scoring: PeerScoreLevel,

    /// Allows to ban peers based on their score.
    ///
    /// Peers are banned based on a ban threshold (see `p2p.ban.threshold`).
    /// If a peer's score is below the threshold, it gets automatically banned.
    pub ban_enabled: bool,

    /// The threshold used to ban peers.
    ///
    /// For peers to be banned, the `p2p.ban.peers` flag must be set to `true`.
    /// By default, peers are banned if their score is below -100. This follows the `op-node` default `<https://github.com/ethereum-optimism/optimism/blob/09a8351a72e43647c8a96f98c16bb60e7b25dc6e/op-node/flags/p2p_flags.go#L123-L130>`.
    pub ban_threshold: i64,

    /// The duration in minutes to ban a peer for.
    ///
    /// For peers to be banned, the `p2p.ban.peers` flag must be set to `true`.
    /// By default peers are banned for 1 hour. This follows the `op-node` default `<https://github.com/ethereum-optimism/optimism/blob/09a8351a72e43647c8a96f98c16bb60e7b25dc6e/op-node/flags/p2p_flags.go#L131-L138>`.
    pub ban_duration: u64,

    /// The interval in seconds to find peers using the discovery service.
    /// Defaults to 5 seconds.
    pub discovery_interval: u64,
    /// The directory to store the bootstore.
    pub bootstore: Option<PathBuf>,
    /// Disables the bootstore.
    pub disable_bootstore: bool,
    /// Peer Redialing threshold is the maximum amount of times to attempt to redial a peer that
    /// disconnects. By default, peers are *not* redialed. If set to 0, the peer will be
    /// redialed indefinitely.
    pub peer_redial: Option<u64>,

    /// The duration in minutes of the peer dial period.
    /// When the last time a peer was dialed is longer than the dial period, the number of peer
    /// dials is reset to 0, allowing the peer to be dialed again.
    pub redial_period: u64,

    /// An optional list of bootnode ENRs or node records to start the node with.
    pub bootnodes: Vec<String>,

    /// Optionally enable topic scoring.
    ///
    /// Topic scoring is a mechanism to score peers based on their behavior in the gossip network.
    /// Historically, topic scoring was only enabled for the v1 topic on the OP Stack p2p network
    /// in the `op-node`. This was a silent bug, and topic scoring is actively being
    /// [phased out of the `op-node`][out].
    ///
    /// This flag is only presented for backwards compatibility and debugging purposes.
    ///
    /// [out]: https://github.com/ethereum-optimism/optimism/pull/15719
    pub topic_scoring: bool,

    /// An optional unsafe block signer address.
    ///
    /// By default, this is fetched from the chain config in the superchain-registry using the
    /// specified L2 chain ID.
    pub unsafe_block_signer: Option<alloy_primitives::Address>,

    /// An optional flag to remove random peers from discovery to rotate the peer set.
    ///
    /// This is the number of seconds to wait before removing a peer from the discovery
    /// service. By default, peers are not removed from the discovery service.
    ///
    /// This is useful for discovering a wider set of peers.
    pub discovery_randomize: Option<u64>,

    /// Specify optional remote signer configuration. Note that this argument is mutually exclusive
    /// with `p2p.sequencer.key` that specifies a local sequencer signer.
    pub signer: SignerArgs,
}

impl Default for P2PArgs {
    fn default() -> Self {
        Self {
            no_discovery: false,
            priv_path: None,
            private_key: None,
            advertise_ip: None,
            advertise_tcp_port: None,
            advertise_udp_port: None,
            listen_ip: "0.0.0.0".parse().unwrap(),
            listen_tcp_port: 9222,
            listen_udp_port: 9223,
            peers_lo: 20,
            peers_hi: 30,
            peers_grace: Duration::from_secs(30),
            gossip_mesh_d: 8,
            gossip_mesh_dlo: 6,
            gossip_mesh_dhi: 12,
            gossip_mesh_dlazy: 6,
            gossip_flood_publish: false,
            scoring: "light".parse().unwrap(),
            ban_enabled: false,
            ban_threshold: -100,
            ban_duration: 60,
            discovery_interval: 5,
            bootstore: None,
            disable_bootstore: false,
            peer_redial: Some(500),
            redial_period: 60,
            bootnodes: Vec::new(),
            topic_scoring: false,
            unsafe_block_signer: None,
            discovery_randomize: None,
            signer: SignerArgs::default(),
        }
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
    pub async fn unsafe_block_signer(
        &self,
        l2_chain_id: u64,
        rollup_config: &RollupConfig,
        l1_eth_rpc: Option<Url>,
        genesis_signer: Option<alloy_primitives::Address>,
    ) -> eyre::Result<alloy_primitives::Address> {
        if let Some(l1_eth_rpc) = l1_eth_rpc {
            /// The storage slot that the unsafe block signer address is stored at.
            /// Computed as: `bytes32(uint256(keccak256("systemconfig.unsafeblocksigner")) - 1)`
            const UNSAFE_BLOCK_SIGNER_ADDRESS_STORAGE_SLOT: B256 =
                b256!("0x65a7ed542fb37fe237fdfbdd70b31598523fe5b32879e307bae27a0bd9581c08");

            let mut provider = AlloyChainProvider::new_http(l1_eth_rpc, 1024);
            let latest_block_num = provider.latest_block_number().await?;
            let block_info = provider.block_info_by_number(latest_block_num).await?;

            // Fetch the unsafe block signer address from the system config.
            let unsafe_block_signer_address = provider
                .inner
                .get_storage_at(
                    rollup_config.l1_system_config_address,
                    UNSAFE_BLOCK_SIGNER_ADDRESS_STORAGE_SLOT.into(),
                )
                .hash(block_info.hash)
                .await?;

            // Convert the unsafe block signer address to the correct type.
            return Ok(alloy_primitives::Address::from_slice(
                &unsafe_block_signer_address.to_be_bytes_vec()[12..],
            ));
        }

        // Otherwise use the genesis signer or the configured unsafe block signer.
        genesis_signer.or(self.unsafe_block_signer).ok_or_else(|| {
            eyre::eyre!(
                "Unsafe block signer not provided for chain ID {}. \
                 Provide --p2p.unsafe.block.signer or ensure the chain is in the superchain registry.",
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
    /// - `genesis_signer`: Optional genesis signer address.
    ///
    /// Errors if the genesis unsafe block signer isn't available for the specified L2 Chain ID.
    pub async fn config(
        self,
        config: &RollupConfig,
        l2_chain_id: u64,
        l1_rpc: Option<Url>,
        genesis_signer: Option<alloy_primitives::Address>,
    ) -> Result<NetworkConfig, P2PConfigError> {
        // Note: the advertised address is contained in the ENR for external peers from the
        // discovery layer to use.

        // Fallback to the listen ip if the advertise ip is not specified
        let advertise_ip = self.advertise_ip.unwrap_or(self.listen_ip);

        // If the advertise ip is set, we will disable the dynamic ENR updates.
        let static_ip = self.advertise_ip.is_some();

        // If the advertise tcp port is null, use the listen tcp port
        let advertise_tcp_port = match self.advertise_tcp_port {
            None => self.listen_tcp_port,
            Some(port) => port,
        };

        let advertise_udp_port = match self.advertise_udp_port {
            None => self.listen_udp_port,
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

        let unsafe_block_signer =
            self.unsafe_block_signer(l2_chain_id, config, l1_rpc, genesis_signer).await?;

        let bootstore =
            if self.disable_bootstore {
                None
            } else {
                Some(self.bootstore.map_or(
                    BootStoreFile::Default { chain_id: l2_chain_id },
                    BootStoreFile::Custom,
                ))
            };

        let bootnodes = self
            .bootnodes
            .iter()
            .map(|bootnode| BootNode::parse_bootnode(bootnode))
            .collect::<Vec<BootNode>>()
            .into();

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
            },
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
}

#[cfg(test)]
mod tests {
    use alloy_primitives::b256;
    use base_consensus_peers::NodeRecord;

    use super::*;

    #[test]
    fn test_p2p_args_keypair_missing_both() {
        let args = P2PArgs::default();
        assert!(args.keypair().is_err());
    }

    #[test]
    fn test_p2p_args_keypair_raw_private_key() {
        let args = P2PArgs {
            private_key: Some(b256!(
                "1d2b0bda21d56b8bd12d4f94ebacffdfb35f5e226f84b461103bb8beab6353be"
            )),
            ..Default::default()
        };
        assert!(args.keypair().is_ok());
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
        let args = P2PArgs {
            priv_path: Some(source_path),
            ..Default::default()
        };
        assert!(args.keypair().is_ok());
    }

    #[test]
    fn test_p2p_args_default() {
        let args = P2PArgs::default();
        assert!(!args.no_discovery);
        assert!(args.priv_path.is_none());
        assert!(args.private_key.is_none());
        assert!(args.advertise_ip.is_none());
        assert_eq!(args.listen_ip, "0.0.0.0".parse::<IpAddr>().unwrap());
        assert_eq!(args.listen_tcp_port, 9222);
        assert_eq!(args.listen_udp_port, 9223);
        assert_eq!(args.peers_lo, 20);
        assert_eq!(args.peers_hi, 30);
        assert_eq!(args.gossip_mesh_d, 8);
        assert_eq!(args.gossip_mesh_dlo, 6);
        assert_eq!(args.gossip_mesh_dhi, 12);
        assert_eq!(args.gossip_mesh_dlazy, 6);
        assert!(!args.gossip_flood_publish);
        assert_eq!(args.ban_threshold, -100);
        assert_eq!(args.ban_duration, 60);
        assert_eq!(args.discovery_interval, 5);
        assert!(args.bootstore.is_none());
        assert!(!args.disable_bootstore);
        assert_eq!(args.peer_redial, Some(500));
        assert_eq!(args.redial_period, 60);
        assert!(args.bootnodes.is_empty());
        assert!(!args.topic_scoring);
        assert!(args.unsafe_block_signer.is_none());
        assert!(args.discovery_randomize.is_none());
    }

    #[test]
    fn test_p2p_args_bootnodes_parse() {
        let args = P2PArgs {
            bootnodes: vec![
                "enode://ca2774c3c401325850b2477fd7d0f27911efbf79b1e8b335066516e2bd8c4c9e0ba9696a94b1cb030a88eac582305ff55e905e64fb77fe0edcd70a4e5296d3ec@34.65.175.185:30305".to_string(),
            ],
            ..Default::default()
        };

        // Parse the bootnodes.
        let bootnodes = args
            .bootnodes
            .iter()
            .map(|bootnode| BootNode::parse_bootnode(bootnode))
            .collect::<Vec<BootNode>>();

        // Otherwise, attempt to use the Node Record format.
        let record = NodeRecord::from_str(
                "enode://ca2774c3c401325850b2477fd7d0f27911efbf79b1e8b335066516e2bd8c4c9e0ba9696a94b1cb030a88eac582305ff55e905e64fb77fe0edcd70a4e5296d3ec@34.65.175.185:30305").unwrap();
        let expected_bootnode = vec![BootNode::from_unsigned(record).unwrap()];

        assert_eq!(bootnodes, expected_bootnode);
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
