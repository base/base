//! Discovery-only bootnode command.

use std::{
    fs,
    net::{IpAddr, SocketAddr, ToSocketAddrs},
    path::PathBuf,
};

use alloy_chains::Chain;
use alloy_primitives::B256;
use base_cli_utils::{LogConfig, RuntimeManager};
use base_client_cli::L2ConfigFile;
use base_common_genesis::RollupConfig;
use base_consensus_disc::{Discv5Builder, LocalNode};
use base_consensus_peers::{BootNode, BootNodes, BootStoreFile, SecretKeyLoader};
use clap::Args;
use discv5::{Config, ConfigBuilder, Enr, enr::k256};
use eyre::Context;
use libp2p::identity::Keypair;
use tokio::time::Duration;
use tracing::{debug, info, warn};

use crate::{
    cli::{LogArgs, MetricsArgs},
    metrics::{init_bootnode_p2p_metrics, init_rollup_config_metrics, record_bootnode_up},
};

/// Base consensus bootnode arguments.
#[derive(Args, Clone, Debug)]
pub struct Bootnode {
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

    /// L2 configuration file.
    #[clap(flatten)]
    pub l2_config: L2ConfigFile,

    /// Bootnode P2P discovery arguments.
    #[command(flatten)]
    pub p2p: BootnodeP2PArgs,
}

/// Prints the deterministic ENR for a Base consensus bootnode.
#[derive(Args, Clone, Debug)]
pub struct BootnodeEnr {
    /// L2 Chain ID or name (8453 = Base Mainnet, 84532 = Base Sepolia).
    #[arg(
        long = "chain",
        short = 'n',
        global = true,
        default_value = "8453",
        env = "BASE_NODE_NETWORK"
    )]
    pub l2_chain_id: Chain,

    /// L2 configuration file.
    #[clap(flatten)]
    pub l2_config: L2ConfigFile,

    /// Bootnode P2P discovery arguments.
    #[command(flatten)]
    pub p2p: BootnodeP2PArgs,
}

impl Bootnode {
    /// Runs the CLI.
    pub fn run(self) -> eyre::Result<()> {
        base_cli_utils::init_tracing!(
            LogConfig::from(self.logging.clone()),
            ["libp2p_gossipsub=error"]
        )?;

        let cfg = self.l2_config.load(&self.l2_chain_id).map_err(|e| eyre::eyre!("{e}"))?;

        base_cli_utils::MetricsConfig::from(self.metrics.clone()).init_with(|| {
            base_cli_utils::register_version_metrics!();
            init_rollup_config_metrics(&cfg);
            init_bootnode_p2p_metrics(&self.p2p);
        })?;

        RuntimeManager::new().run_until_ctrl_c(self.exec(cfg))
    }

    /// Runs the discovery-only bootnode.
    pub async fn exec(self, cfg: RollupConfig) -> eyre::Result<()> {
        let chain_id = cfg.l2_chain_id.id();
        self.p2p.check_ports()?;

        let driver = self.p2p.discovery_driver(chain_id)?;
        let (handler, mut discovered_enrs) = driver.start();
        let local_enr = handler.local_enr().await.wrap_err("discovery service stopped")?;
        self.p2p.write_enr_output(&local_enr)?;

        info!(
            target: "rollup_node::bootnode",
            chain_id = chain_id,
            enr = %local_enr,
            "Consensus bootnode started"
        );
        record_bootnode_up();

        while let Some(enr) = discovered_enrs.recv().await {
            debug!(
                target: "rollup_node::bootnode",
                peer_id = %enr.node_id(),
                enr = %enr,
                "Discovered consensus peer"
            );
        }

        warn!(target: "rollup_node::bootnode", "Discovery ENR stream closed");
        Ok(())
    }
}

impl BootnodeEnr {
    /// Runs the CLI.
    pub fn run(self) -> eyre::Result<()> {
        let cfg = self.l2_config.load(&self.l2_chain_id).map_err(|e| eyre::eyre!("{e}"))?;
        let enr = self.p2p.local_enr(cfg.l2_chain_id.id())?;
        self.p2p.write_enr_output(&enr)?;
        println!("{enr}");
        Ok(())
    }
}

/// P2P discovery arguments for the consensus bootnode.
#[derive(Args, Clone, Debug, PartialEq, Eq)]
pub struct BootnodeP2PArgs {
    /// Read the hex-encoded 32-byte private key for the peer ID from this txt file.
    ///
    /// The file is created if it does not already exist.
    /// If omitted, the bootnode uses `~/.base/<chain_id>/bootnode_p2p_priv.txt`.
    #[arg(long = "p2p.priv.path", env = "BASE_NODE_P2P_PRIV_PATH")]
    pub priv_path: Option<PathBuf>,

    /// The hex-encoded 32-byte private key for the peer ID.
    #[arg(long = "p2p.priv.raw", env = "BASE_NODE_P2P_PRIV_RAW")]
    pub private_key: Option<B256>,

    /// IP address or DNS hostname to advertise to external peers from Discv5.
    #[arg(long = "p2p.advertise.ip", env = "BASE_NODE_P2P_ADVERTISE_IP", value_parser = resolve_host)]
    pub advertise_ip: Option<IpAddr>,

    /// TCP port to advertise to external peers from the discovery layer.
    ///
    /// If omitted, the bootnode uses `p2p.listen.tcp`.
    #[arg(long = "p2p.advertise.tcp", env = "BASE_NODE_P2P_ADVERTISE_TCP_PORT")]
    pub advertise_tcp_port: Option<u16>,

    /// UDP port to advertise to external peers from the discovery layer.
    ///
    /// If omitted, the bootnode uses `p2p.listen.udp`.
    #[arg(long = "p2p.advertise.udp", env = "BASE_NODE_P2P_ADVERTISE_UDP_PORT")]
    pub advertise_udp_port: Option<u16>,

    /// IP address or DNS hostname to bind Discv5 to.
    #[arg(long = "p2p.listen.ip", default_value = "0.0.0.0", env = "BASE_NODE_P2P_LISTEN_IP", value_parser = resolve_host)]
    pub listen_ip: IpAddr,

    /// TCP port to advertise in the local ENR.
    #[arg(long = "p2p.listen.tcp", default_value = "9222", env = "BASE_NODE_P2P_LISTEN_TCP_PORT")]
    pub listen_tcp_port: u16,

    /// UDP port to bind Discv5 to.
    #[arg(long = "p2p.listen.udp", default_value = "9223", env = "BASE_NODE_P2P_LISTEN_UDP_PORT")]
    pub listen_udp_port: u16,

    /// The interval in seconds to find peers using the discovery service.
    #[arg(
        long = "p2p.discovery.interval",
        default_value = "5",
        env = "BASE_NODE_P2P_DISCOVERY_INTERVAL"
    )]
    pub discovery_interval: u64,

    /// Path to the bootstore file.
    #[arg(long = "p2p.bootstore", env = "BASE_NODE_P2P_BOOTSTORE")]
    pub bootstore: Option<PathBuf>,

    /// Disables the bootstore.
    #[arg(long = "p2p.no-bootstore", env = "BASE_NODE_P2P_NO_BOOTSTORE")]
    pub disable_bootstore: bool,

    /// An optional list of bootnode ENRs or node records to start the node with.
    #[arg(long = "p2p.bootnodes", value_delimiter = ',', env = "BASE_NODE_P2P_BOOTNODES")]
    pub bootnodes: Vec<String>,

    /// Optionally remove random peers from discovery to rotate the peer set.
    #[arg(long = "p2p.discovery.randomize", env = "BASE_NODE_P2P_DISCOVERY_RANDOMIZE")]
    pub discovery_randomize: Option<u64>,

    /// Path to write the local bootnode ENR after it is materialized.
    #[arg(long = "p2p.enr-output", env = "BASE_NODE_P2P_ENR_OUTPUT")]
    pub enr_output: Option<PathBuf>,
}

impl Default for BootnodeP2PArgs {
    fn default() -> Self {
        Self {
            priv_path: None,
            private_key: None,
            advertise_ip: None,
            advertise_tcp_port: None,
            advertise_udp_port: None,
            listen_ip: "0.0.0.0".parse().expect("valid default IP"),
            listen_tcp_port: 9222,
            listen_udp_port: 9223,
            discovery_interval: 5,
            bootstore: None,
            disable_bootstore: false,
            bootnodes: Vec::new(),
            discovery_randomize: None,
            enr_output: None,
        }
    }
}

impl BootnodeP2PArgs {
    /// Checks if the configured listen port is available on the system.
    fn check_ports(&self) -> eyre::Result<()> {
        if self.listen_udp_port == 0 {
            return Ok(());
        }

        std::net::UdpSocket::bind((self.listen_ip, self.listen_udp_port))
            .wrap_err_with(|| format!("Error binding UDP socket on port {}", self.listen_udp_port))
            .map(drop)
    }

    /// Builds the discovery driver from the CLI arguments.
    fn discovery_driver(&self, chain_id: u64) -> eyre::Result<base_consensus_disc::Discv5Driver> {
        let keypair = self.keypair(chain_id)?;
        let local_node_key = Self::local_node_key(keypair)?;
        let advertised = self.advertised_node(local_node_key);
        let discovery_config = self.discovery_config();
        let bootstore = self.bootstore(chain_id);
        let bootnodes = self.bootnodes()?;

        let mut builder = Discv5Builder::new(advertised, chain_id, discovery_config)
            .with_bootstore_file(bootstore)
            .with_bootnodes(bootnodes)
            .with_interval(Duration::from_secs(self.discovery_interval))
            .disable_forward();

        if let Some(randomize) = self.discovery_randomize {
            builder = builder.with_discovery_randomize(Some(Duration::from_secs(randomize)));
        }

        builder.build().map_err(Into::into)
    }

    /// Builds the local bootnode ENR without starting the discovery service.
    pub fn local_enr(&self, chain_id: u64) -> eyre::Result<Enr> {
        let keypair = self.keypair(chain_id)?;
        let local_node_key = Self::local_node_key(keypair)?;
        self.advertised_node(local_node_key)
            .build_enr(chain_id)
            .map_err(|e| eyre::eyre!("Failed to build bootnode ENR: {e}"))
    }

    /// Writes the local ENR to `--p2p.enr-output`, if configured.
    pub fn write_enr_output(&self, enr: &Enr) -> eyre::Result<()> {
        let Some(path) = &self.enr_output else {
            return Ok(());
        };

        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent).wrap_err_with(|| {
                format!("Failed to create ENR output directory {}", parent.display())
            })?;
        }

        fs::write(path, format!("{enr}\n"))
            .wrap_err_with(|| format!("Failed to write bootnode ENR to {}", path.display()))?;
        info!(target: "rollup_node::bootnode", path = %path.display(), "Wrote bootnode ENR");
        Ok(())
    }

    fn keypair(&self, chain_id: u64) -> eyre::Result<Keypair> {
        if let Some(mut private_key) = self.private_key {
            return SecretKeyLoader::parse(&mut private_key.0).map_err(Into::into);
        }

        let key_path = self.priv_path.clone().unwrap_or_else(|| Self::default_key_path(chain_id));
        info!(target: "rollup_node::bootnode", path = %key_path.display(), "Using bootnode P2P key path");
        SecretKeyLoader::load(&key_path).map_err(Into::into)
    }

    fn default_key_path(chain_id: u64) -> PathBuf {
        let mut path = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
        path.push(".base");
        path.push(chain_id.to_string());
        path.push("bootnode_p2p_priv.txt");
        path
    }

    fn local_node_key(keypair: Keypair) -> eyre::Result<k256::ecdsa::SigningKey> {
        let secp256k1_key = keypair
            .try_into_secp256k1()
            .map_err(|e| eyre::eyre!("P2P keypair must be secp256k1: {e}"))?
            .secret()
            .to_bytes();

        k256::ecdsa::SigningKey::from_bytes(&secp256k1_key.into())
            .map_err(|e| eyre::eyre!("Failed to convert P2P keypair into discv5 signing key: {e}"))
    }

    fn advertised_node(&self, signing_key: k256::ecdsa::SigningKey) -> LocalNode {
        LocalNode::new(
            signing_key,
            self.advertised_ip(),
            self.advertised_tcp_port(),
            self.advertised_udp_port(),
        )
    }

    pub(crate) fn advertised_ip(&self) -> IpAddr {
        self.advertise_ip.unwrap_or(self.listen_ip)
    }

    pub(crate) fn advertised_tcp_port(&self) -> u16 {
        self.advertise_tcp_port.unwrap_or(self.listen_tcp_port)
    }

    pub(crate) fn advertised_udp_port(&self) -> u16 {
        self.advertise_udp_port.unwrap_or(self.listen_udp_port)
    }

    fn discovery_config(&self) -> Config {
        let listen_config = SocketAddr::new(self.listen_ip, self.listen_udp_port).into();
        let mut builder = ConfigBuilder::new(listen_config);

        if self.advertise_ip.is_some() {
            builder.disable_enr_update();
            builder.auto_nat_listen_duration(None);
        }

        builder.build()
    }

    fn bootstore(&self, chain_id: u64) -> Option<BootStoreFile> {
        if self.disable_bootstore {
            None
        } else {
            Some(
                self.bootstore
                    .clone()
                    .map_or(BootStoreFile::Default { chain_id }, BootStoreFile::Custom),
            )
        }
    }

    fn bootnodes(&self) -> eyre::Result<BootNodes> {
        self.bootnodes
            .iter()
            .map(|bootnode| {
                BootNode::parse_bootnode(bootnode)
                    .map_err(|e| eyre::eyre!("Failed to parse bootnode '{bootnode}': {e}"))
            })
            .collect::<eyre::Result<Vec<BootNode>>>()
            .map(Into::into)
    }
}

fn resolve_host(host: &str) -> Result<IpAddr, String> {
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(ip);
    }

    let socket_addr = format!("{host}:0");
    socket_addr
        .to_socket_addrs()
        .map_err(|e| format!("Failed to resolve '{host}': {e}"))?
        .next()
        .map(|addr| addr.ip())
        .ok_or_else(|| format!("DNS resolution for '{host}' returned no addresses"))
}

#[cfg(test)]
mod tests {
    use std::{net::Ipv4Addr, path::PathBuf};

    use alloy_primitives::b256;
    use base_consensus_peers::EnrValidation;
    use clap::Parser;

    use super::*;

    #[derive(Parser)]
    struct TestCommand {
        #[command(flatten)]
        p2p: BootnodeP2PArgs,
    }

    fn p2p_args(args: &[&str]) -> BootnodeP2PArgs {
        let args = [&["test"], args].concat();
        TestCommand::parse_from(args).p2p
    }

    #[test]
    fn advertised_address_defaults_to_listen_address() {
        let p2p = p2p_args(&[
            "--p2p.listen.ip",
            "127.0.0.1",
            "--p2p.listen.tcp",
            "9224",
            "--p2p.listen.udp",
            "9225",
        ]);

        assert_eq!(p2p.advertised_ip(), IpAddr::V4(Ipv4Addr::LOCALHOST));
        assert_eq!(p2p.advertised_tcp_port(), 9224);
        assert_eq!(p2p.advertised_udp_port(), 9225);
    }

    #[test]
    fn advertised_address_uses_static_overrides() {
        let p2p = p2p_args(&[
            "--p2p.listen.ip",
            "127.0.0.1",
            "--p2p.advertise.ip",
            "192.0.2.1",
            "--p2p.advertise.tcp",
            "30303",
            "--p2p.advertise.udp",
            "30304",
        ]);

        assert_eq!(p2p.advertised_ip(), IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1)));
        assert_eq!(p2p.advertised_tcp_port(), 30303);
        assert_eq!(p2p.advertised_udp_port(), 30304);
    }

    #[test]
    fn default_key_path_is_chain_scoped() {
        let path = BootnodeP2PArgs::default_key_path(8453);

        assert!(path.ends_with(".base/8453/bootnode_p2p_priv.txt"));
    }

    #[test]
    fn parses_raw_key_into_keypair() {
        let p2p = BootnodeP2PArgs {
            private_key: Some(b256!(
                "1d2b0bda21d56b8bd12d4f94ebacffdfb35f5e226f84b461103bb8beab6353be"
            )),
            ..Default::default()
        };

        assert!(p2p.keypair(8453).is_ok());
    }

    #[test]
    fn local_enr_uses_advertised_address_and_valid_chain_metadata() {
        let p2p = BootnodeP2PArgs {
            private_key: Some(b256!(
                "1d2b0bda21d56b8bd12d4f94ebacffdfb35f5e226f84b461103bb8beab6353be"
            )),
            advertise_ip: Some(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 10))),
            listen_tcp_port: 30303,
            listen_udp_port: 30304,
            ..Default::default()
        };

        let enr = p2p.local_enr(84538453).expect("ENR should build");

        assert_eq!(enr.ip4(), Some(Ipv4Addr::new(192, 0, 2, 10)));
        assert_eq!(enr.tcp4(), Some(30303));
        assert_eq!(enr.udp4(), Some(30304));
        assert!(EnrValidation::validate(&enr, 84538453).is_valid());
    }

    #[test]
    fn writes_enr_output_file() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let output = dir.path().join("nested").join("bootnode.enr");
        let p2p = BootnodeP2PArgs {
            private_key: Some(b256!(
                "1d2b0bda21d56b8bd12d4f94ebacffdfb35f5e226f84b461103bb8beab6353be"
            )),
            enr_output: Some(output.clone()),
            ..Default::default()
        };
        let enr = p2p.local_enr(84538453).expect("ENR should build");

        p2p.write_enr_output(&enr).expect("ENR output should be written");

        let written = std::fs::read_to_string(output).expect("ENR output should be readable");
        assert_eq!(written, format!("{enr}\n"));
    }

    #[test]
    fn bootstore_defaults_to_chain_scoped_file() {
        let p2p = BootnodeP2PArgs::default();

        assert_eq!(p2p.bootstore(8453), Some(BootStoreFile::Default { chain_id: 8453 }));
    }

    #[test]
    fn bootstore_uses_custom_path() {
        let path = PathBuf::from("/tmp/bootstore.json");
        let p2p = BootnodeP2PArgs { bootstore: Some(path.clone()), ..Default::default() };

        assert_eq!(p2p.bootstore(8453), Some(BootStoreFile::Custom(path)));
    }

    #[test]
    fn bootstore_can_be_disabled() {
        let p2p = BootnodeP2PArgs {
            bootstore: Some(PathBuf::from("/tmp/bootstore.json")),
            disable_bootstore: true,
            ..Default::default()
        };

        assert_eq!(p2p.bootstore(8453), None);
    }

    #[test]
    fn invalid_bootnode_errors() {
        let p2p = p2p_args(&["--p2p.bootnodes", "enr:invalid"]);
        let err = p2p.bootnodes().expect_err("invalid bootnode should fail");

        assert!(err.to_string().contains("Failed to parse bootnode 'enr:invalid'"));
    }

    #[test]
    fn parses_enr_output_path() {
        let p2p = p2p_args(&["--p2p.enr-output", "/tmp/base-cl-bootnode.enr"]);

        assert_eq!(p2p.enr_output, Some(PathBuf::from("/tmp/base-cl-bootnode.enr")));
    }
}
