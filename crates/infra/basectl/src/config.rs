use std::path::PathBuf;

use alloy_primitives::Address;
use alloy_provider::{Provider, ProviderBuilder};
use anyhow::{Context, Result};
use base_common_chains::{ChainConfig, rollup_config};
use base_common_genesis::{RollupConfig, UpgradeConfig};
use serde::{Deserialize, Serialize};
use tracing::warn;
use url::Url;

/// Configuration for one Kubernetes pod group rendered by the pods view.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PodGroupConfig {
    /// Short group alias shown in compact tables.
    pub alias: String,
    /// Human-readable group label.
    pub label: String,
    /// Kubernetes context passed to `kubectl --context`.
    pub context: String,
    /// Kubernetes namespace passed to `kubectl --namespace`.
    pub namespace: String,
    /// Optional Kubernetes label selector passed to `kubectl -l`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selector: Option<String>,
}

/// Configuration for the Kubernetes pods view.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PodsConfig {
    /// Optional path to a `kubectl` executable. Defaults to `kubectl` from `PATH`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kubectl: Option<PathBuf>,
    /// How often to refresh pod status, in milliseconds.
    #[serde(default = "default_pods_refresh_interval_ms")]
    pub refresh_interval_ms: u64,
    /// Static pod groups from the user's local config file.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<PodGroupConfig>,
}

impl PodsConfig {
    /// Returns the command used to invoke Kubernetes.
    pub fn kubectl_program(&self) -> PathBuf {
        self.kubectl.clone().unwrap_or_else(|| PathBuf::from("kubectl"))
    }

    /// Returns the configured refresh interval, never less than 250 ms.
    pub const fn refresh_interval(&self) -> std::time::Duration {
        let millis = if self.refresh_interval_ms < 250 { 250 } else { self.refresh_interval_ms };
        std::time::Duration::from_millis(millis)
    }
}

const fn default_pods_refresh_interval_ms() -> u64 {
    1_000
}

/// Configuration for proof system monitoring (proposer + dispute games).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProofsConfig {
    /// Address of the `DisputeGameFactory` contract on L1.
    pub dispute_game_factory: Address,
    /// Address of the `AnchorStateRegistry` contract on L1.
    pub anchor_state_registry: Address,
}

/// Configuration for a single validator (non-sequencing) node in the local devnet.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidatorNodeConfig {
    /// Human-readable name for this node (e.g. "base-client").
    pub name: String,
    /// Human-readable binary/process description shown in the TUI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary: Option<String>,
    /// Consensus-layer JSON-RPC endpoint (serves `optimism_*` and `opp2p_*` methods).
    pub cl_rpc: Url,
    /// Execution-layer JSON-RPC endpoint for this node.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub el_rpc: Option<Url>,
    /// Docker container name for the EL process.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub docker_el: Option<String>,
    /// Docker container name for the CL process.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub docker_cl: Option<String>,
}

/// Configuration for a single node in an HA conductor cluster.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConductorNodeConfig {
    /// Human-readable name for this node (e.g. "op-conductor-0").
    pub name: String,
    /// Conductor JSON-RPC endpoint (serves `conductor_*` methods).
    pub conductor_rpc: Url,
    /// Consensus-layer JSON-RPC endpoint (serves `optimism_*` and `opp2p_*` methods).
    pub cl_rpc: Url,
    /// Raft server ID used when targeting this node for leadership transfer.
    pub server_id: String,
    /// Raft peer address (`host:port`) used when targeting this node for leadership transfer.
    pub raft_addr: String,
    /// Execution-layer JSON-RPC endpoint for this sequencer's EL node.
    ///
    /// If set, the TUI polls `net_peerCount` on this endpoint to show the EL
    /// peer count separately from the CL P2P peer count.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub el_rpc: Option<Url>,
    /// Docker container name for the conductor process.
    ///
    /// If set, the TUI can restart this container with `r`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub docker_conductor: Option<String>,
    /// Docker container name for the EL (execution layer) process.
    ///
    /// If set, the TUI can restart this container with `r`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub docker_el: Option<String>,
    /// Docker container name for the CL (consensus layer) process.
    ///
    /// If set, the TUI can restart this container with `r`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub docker_cl: Option<String>,
    /// Flashblocks WebSocket endpoint for this sequencer's builder node.
    ///
    /// When set, the command center will automatically reconnect its flashblocks
    /// stream to the current Raft leader's endpoint whenever leadership changes,
    /// rather than staying connected to the original leader's now-idle socket.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flashblocks_ws: Option<Url>,
}

/// Conductor cluster discovery configuration.
///
/// When set, basectl can bootstrap a conductor cluster view from a single
/// RPC endpoint by calling `conductor_clusterMembership` and synthesising
/// per-peer `ConductorNodeConfig` entries via [`DiscoveryPorts`] templates.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveryConfig {
    /// Bootstrap conductor RPC URL.
    ///
    /// basectl will hit this URL first to learn the live raft membership and
    /// then poll all discovered peers. May be overridden by `--conductor-rpc`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bootstrap_rpc: Option<Url>,
    /// Port templates used when rebuilding per-peer JSON-RPC URLs from the
    /// raft binary protocol addresses returned by `conductor_clusterMembership`.
    #[serde(default)]
    pub ports: DiscoveryPorts,
}

/// Port templates used to derive per-peer JSON-RPC URLs from raft addresses.
///
/// `conductor_clusterMembership` returns each peer's *raft binary protocol*
/// address (e.g. `op-conductor-1:5051`), not its JSON-RPC URL. basectl extracts
/// the host and rebuilds JSON-RPC URLs for each service using these ports.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveryPorts {
    /// Conductor JSON-RPC port (default 5545).
    #[serde(default = "default_conductor_rpc_port")]
    pub conductor_rpc: u16,
    /// Consensus-layer JSON-RPC port (default 7545).
    #[serde(default = "default_cl_rpc_port")]
    pub cl_rpc: u16,
    /// Execution-layer JSON-RPC port (default 8545). When `None`, EL data is
    /// not polled for discovered peers and shows as `—` in the UI.
    #[serde(default = "default_el_rpc_port", skip_serializing_if = "Option::is_none")]
    pub el_rpc: Option<u16>,
}

impl Default for DiscoveryPorts {
    fn default() -> Self {
        Self {
            conductor_rpc: default_conductor_rpc_port(),
            cl_rpc: default_cl_rpc_port(),
            el_rpc: default_el_rpc_port(),
        }
    }
}

const fn default_conductor_rpc_port() -> u16 {
    5545
}

const fn default_cl_rpc_port() -> u16 {
    7545
}

const fn default_el_rpc_port() -> Option<u16> {
    Some(8545)
}

/// Origin of the conductor cluster node list used by the poller.
///
/// `Static` is the original behaviour: the YAML/devnet config enumerates every
/// node up front. `Discover` bootstraps from a single conductor RPC URL and
/// rebuilds the peer list each tick from `conductor_clusterMembership`.
#[derive(Debug, Clone)]
pub enum ConductorSource {
    /// Hand-configured node list (devnet, custom YAML).
    Static(Vec<ConductorNodeConfig>),
    /// Bootstrap from a single conductor RPC and derive peers via port templates.
    Discover {
        /// Bootstrap conductor RPC URL.
        bootstrap: Url,
        /// Port templates for rebuilding per-peer JSON-RPC URLs.
        ports: DiscoveryPorts,
    },
}

impl ConductorSource {
    /// Returns `true` if this source bootstraps from a single RPC.
    pub const fn is_discover(&self) -> bool {
        matches!(self, Self::Discover { .. })
    }

    /// Returns an ephemeral single-node config for the bootstrap URL.
    ///
    /// Used on the very first poll cycle of a `Discover` source, before
    /// `conductor_clusterMembership` has returned anything. Once membership
    /// is known, [`ConductorSource::synthesize_nodes`] takes over.
    pub fn bootstrap_node(&self) -> Option<ConductorNodeConfig> {
        match self {
            Self::Static(_) => None,
            Self::Discover { bootstrap, ports } => {
                let host = bootstrap.host_str().unwrap_or("localhost");
                let cl_rpc = peer_url(bootstrap, host, ports.cl_rpc);
                let el_rpc = ports.el_rpc.map(|p| peer_url(bootstrap, host, p));
                Some(ConductorNodeConfig {
                    name: "local".to_string(),
                    conductor_rpc: bootstrap.clone(),
                    cl_rpc,
                    server_id: "local".to_string(),
                    raft_addr: String::new(),
                    el_rpc,
                    docker_conductor: None,
                    docker_el: None,
                    docker_cl: None,
                    flashblocks_ws: None,
                })
            }
        }
    }

    /// Synthesises per-peer `ConductorNodeConfig` entries from raft membership.
    ///
    /// Returns `None` for [`ConductorSource::Static`] (those nodes are already
    /// fully configured). For [`ConductorSource::Discover`], each `ServerInfo`
    /// in `membership` has an `addr` field that is the raft binary protocol
    /// address (e.g. `op-conductor-1:5051`); the host is extracted and the
    /// JSON-RPC URLs are rebuilt from the supplied port templates. Docker
    /// container names are left `None` because the local docker daemon can't
    /// reach remote peers' containers; restart is also UI-disabled for
    /// discovered peers.
    pub fn synthesize_nodes(
        &self,
        membership: &base_consensus_rpc::ClusterMembership,
    ) -> Option<Vec<ConductorNodeConfig>> {
        let Self::Discover { bootstrap, ports } = self else { return None };
        let nodes = membership
            .servers
            .iter()
            .map(|srv| {
                let host = srv.addr.split(':').next().unwrap_or(srv.addr.as_str());
                ConductorNodeConfig {
                    name: srv.id.clone(),
                    conductor_rpc: peer_url(bootstrap, host, ports.conductor_rpc),
                    cl_rpc: peer_url(bootstrap, host, ports.cl_rpc),
                    server_id: srv.id.clone(),
                    raft_addr: srv.addr.clone(),
                    el_rpc: ports.el_rpc.map(|p| peer_url(bootstrap, host, p)),
                    docker_conductor: None,
                    docker_el: None,
                    docker_cl: None,
                    flashblocks_ws: None,
                }
            })
            .collect();
        Some(nodes)
    }
}

/// Builds a peer JSON-RPC URL by string interpolation against `bootstrap`'s scheme.
///
/// Falls back to a clone of `bootstrap` and logs a warning if the resulting
/// URL fails to parse (e.g. an unexpected host shape coming back from raft).
/// Returning `bootstrap` is a safer default than panicking — the poll will
/// just hit the bootstrap node twice, which is visible to the operator.
fn peer_url(bootstrap: &Url, host: &str, port: u16) -> Url {
    let scheme = bootstrap.scheme();
    let candidate = format!("{scheme}://{host}:{port}");
    match Url::parse(&candidate) {
        Ok(url) => url,
        Err(error) => {
            warn!(host = %host, port = port, error = %error, "discovered peer host failed url parse; falling back to bootstrap");
            bootstrap.clone()
        }
    }
}

/// Monitoring configuration for a chain watched by basectl.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MonitoringConfig {
    /// Human-readable chain name (e.g. "mainnet", "sepolia").
    pub name: String,
    /// L2 JSON-RPC endpoint URL.
    pub rpc: Url,
    /// Flashblocks WebSocket endpoint URL.
    pub flashblocks_ws: Url,
    /// L1 Ethereum JSON-RPC endpoint URL.
    pub l1_rpc: Url,
    /// Optional Base consensus node JSON-RPC endpoint URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consensus_node_rpc: Option<Url>,
    /// Live rollup upgrade configuration fetched from the consensus node when available.
    #[serde(default, skip_serializing_if = "Option::is_none", alias = "hardforks")]
    pub upgrades: Option<UpgradeConfig>,
    /// L1 `SystemConfig` contract address.
    pub system_config: Address,
    /// L1 batcher address for blob attribution.
    ///
    /// This is the current live batcher address, not necessarily the genesis
    /// batcher. It may differ from the value in `base-common-chains` if
    /// the batcher was updated via a `SystemConfig` transaction after genesis.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batcher_address: Option<Address>,
    /// Expected number of blobs per L1 block target.
    #[serde(default = "default_blob_target")]
    pub l1_blob_target: u64,
    /// HA conductor cluster nodes, if this chain runs an op-conductor setup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conductors: Option<Vec<ConductorNodeConfig>>,
    /// Bootstrap configuration for runtime conductor cluster discovery.
    ///
    /// Used when `conductors` is `None` (or the operator passes
    /// `--conductor-rpc`) to derive the peer list from a single bootstrap RPC.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discovery: Option<DiscoveryConfig>,
    /// Validator (non-sequencing) nodes to monitor alongside the conductor cluster.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub validators: Option<Vec<ValidatorNodeConfig>>,
    /// Proof system monitoring configuration (dispute games, anchor state).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proofs: Option<ProofsConfig>,
    /// Kubernetes pod groups to display in the pods view.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pods: Option<PodsConfig>,
}

impl MonitoringConfig {
    /// Returns the block explorer base URL for this chain, if known.
    pub fn explorer_base_url(&self) -> Option<&'static str> {
        match self.name.as_str() {
            "mainnet" => Some("https://basescan.org"),
            "sepolia" => Some("https://sepolia.basescan.org"),
            _ => None,
        }
    }

    /// Returns the L1 explorer base URL for this chain, if known.
    pub fn l1_explorer_base_url(&self) -> Option<&'static str> {
        match self.name.as_str() {
            "mainnet" => Some("https://etherscan.io"),
            "sepolia" => Some("https://sepolia.etherscan.io"),
            _ => None,
        }
    }

    /// Returns the basectl display name for a known Base chain ID.
    ///
    /// Maps 8453/84532/763360 to `"mainnet"`/`"sepolia"`/`"zeronet"` so the
    /// network badge agrees with what `-c` accepts on the CLI.
    pub const fn name_for_chain_id(chain_id: u64) -> Option<&'static str> {
        match chain_id {
            8453 => Some("mainnet"),
            84532 => Some("sepolia"),
            763360 => Some("zeronet"),
            _ => None,
        }
    }

    /// Detects the live network name by calling `eth_chainId` on the L2 RPC.
    ///
    /// Returns the basectl-style name (e.g. `"mainnet"`) for known Base chain
    /// IDs, or `None` when the RPC is unreachable or the chain ID is unknown.
    pub async fn detect_name_from_rpc(rpc: &Url) -> Option<String> {
        let provider = ProviderBuilder::new().connect(rpc.as_str()).await.ok()?;
        let chain_id = provider.get_chain_id().await.ok()?;
        Self::name_for_chain_id(chain_id).map(str::to_owned)
    }

    /// Returns the URL to use for `eth_chainId` network detection.
    ///
    /// When `conductor_rpc` is `Some`, derives the EL URL from the bootstrap
    /// host and the discovery EL port template, so the badge reflects the
    /// cluster basectl was pointed at instead of the preset's default RPC.
    /// Falls back to `self.rpc` when URL construction fails or no bootstrap
    /// is provided.
    pub fn detect_rpc_for(&self, conductor_rpc: Option<&Url>) -> Url {
        let Some(bootstrap) = conductor_rpc else { return self.rpc.clone() };
        let el_port = self.discovery.as_ref().and_then(|d| d.ports.el_rpc).unwrap_or(8545);
        let mut candidate = bootstrap.clone();
        if candidate.set_port(Some(el_port)).is_err() {
            return self.rpc.clone();
        }
        candidate
    }

    /// Resolves the active conductor source from CLI flag and config.
    ///
    /// Precedence: hand-configured `conductors` list > CLI `--conductor-rpc`
    /// flag > `discovery.bootstrap_rpc` from config.
    pub fn conductor_source(&self, cli_flag: Option<Url>) -> Option<ConductorSource> {
        if let Some(nodes) = self.conductors.clone() {
            return Some(ConductorSource::Static(nodes));
        }
        if let Some(bootstrap) = cli_flag {
            let ports = self.discovery.as_ref().map(|d| d.ports.clone()).unwrap_or_default();
            return Some(ConductorSource::Discover { bootstrap, ports });
        }
        if let Some(d) = self.discovery.as_ref()
            && let Some(bootstrap) = d.bootstrap_rpc.clone()
        {
            return Some(ConductorSource::Discover { bootstrap, ports: d.ports.clone() });
        }
        None
    }
}

const fn default_blob_target() -> u64 {
    14
}

#[derive(Debug, Clone, Deserialize, Default)]
struct MonitoringConfigOverride {
    name: Option<String>,
    rpc: Option<Url>,
    flashblocks_ws: Option<Url>,
    l1_rpc: Option<Url>,
    consensus_node_rpc: Option<Url>,
    #[serde(alias = "hardforks")]
    upgrades: Option<UpgradeConfig>,
    #[serde(default)]
    system_config: Option<Address>,
    #[serde(default)]
    batcher_address: Option<Address>,
    l1_blob_target: Option<u64>,
    conductors: Option<Vec<ConductorNodeConfig>>,
    discovery: Option<DiscoveryConfig>,
    validators: Option<Vec<ValidatorNodeConfig>>,
    proofs: Option<ProofsConfig>,
    pods: Option<PodsConfig>,
}

impl MonitoringConfig {
    /// Returns a sorted list of all available network names: the three built-ins
    /// followed by any `*.yaml`/`*.yml` files found in `~/.config/base/networks/`
    /// that are not already covered by the built-ins.
    pub fn available_names() -> Vec<String> {
        let mut names = vec!["mainnet".to_string(), "sepolia".to_string(), "devnet".to_string()];
        if let Some(dir) = Self::config_dir()
            && let Ok(entries) = std::fs::read_dir(&dir)
        {
            for entry in entries.flatten() {
                let path = entry.path();
                let ext = path.extension().and_then(|e| e.to_str()).map(str::to_owned);
                if matches!(ext.as_deref(), Some("yaml") | Some("yml"))
                    && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
                {
                    let s = stem.to_string();
                    if !names.contains(&s) {
                        names.push(s);
                    }
                }
            }
        }
        names
    }

    /// Returns the default Base mainnet configuration.
    pub fn mainnet() -> Self {
        let rollup = rollup_config!(ChainConfig::MAINNET);
        Self {
            name: "mainnet".to_string(),
            rpc: Url::parse("https://mainnet.base.org").unwrap(),
            flashblocks_ws: Url::parse("wss://mainnet.flashblocks.base.org/ws").unwrap(),
            l1_rpc: Url::parse("https://ethereum-rpc.publicnode.com").unwrap(),
            consensus_node_rpc: None,
            upgrades: Some(rollup.upgrades),
            system_config: rollup.l1_system_config_address,
            batcher_address: Some("0x5050F69a9786F081509234F1a7F4684b5E5b76C9".parse().unwrap()),
            l1_blob_target: 14,
            conductors: None,
            discovery: Some(DiscoveryConfig {
                bootstrap_rpc: None,
                ports: DiscoveryPorts::default(),
            }),
            validators: None,
            proofs: None,
            pods: None,
        }
    }

    /// Returns the default Base Sepolia configuration.
    pub fn sepolia() -> Self {
        let rollup = rollup_config!(ChainConfig::SEPOLIA);
        Self {
            name: "sepolia".to_string(),
            rpc: Url::parse("https://sepolia.base.org").unwrap(),
            flashblocks_ws: Url::parse("wss://sepolia.flashblocks.base.org/ws").unwrap(),
            l1_rpc: Url::parse("https://ethereum-sepolia-rpc.publicnode.com").unwrap(),
            consensus_node_rpc: None,
            upgrades: Some(rollup.upgrades),
            system_config: rollup.l1_system_config_address,
            batcher_address: Some("0xfc56E7272EEBBBA5bC6c544e159483C4a38f8bA3".parse().unwrap()),
            l1_blob_target: 14,
            conductors: None,
            discovery: Some(DiscoveryConfig {
                bootstrap_rpc: None,
                ports: DiscoveryPorts::default(),
            }),
            validators: None,
            proofs: None,
            pods: None,
        }
    }

    /// Returns a devnet configuration for local development.
    ///
    /// The devnet addresses are fetched dynamically from the consensus node via the
    /// `optimism_rollupConfig` RPC method since they are regenerated each time
    /// the devnet is started.
    ///
    /// Use `load("devnet")` to get a fully configured devnet with addresses
    /// fetched from the running consensus node.
    fn devnet_base() -> Self {
        Self {
            name: "devnet".to_string(),
            rpc: Url::parse("http://localhost:7545").unwrap(),
            flashblocks_ws: Url::parse("ws://localhost:7111").unwrap(),
            l1_rpc: Url::parse("http://localhost:4545").unwrap(),
            consensus_node_rpc: Some(Url::parse("http://localhost:7549").unwrap()),
            upgrades: None,
            // These will be populated by fetch_rollup_config
            system_config: Address::ZERO,
            batcher_address: None,
            l1_blob_target: 14,
            conductors: Some(vec![
                ConductorNodeConfig {
                    name: "op-conductor-0".to_string(),
                    conductor_rpc: Url::parse("http://localhost:6545").unwrap(),
                    cl_rpc: Url::parse("http://localhost:7549").unwrap(),
                    server_id: "sequencer-0".to_string(),
                    raft_addr: "op-conductor-0:5050".to_string(),
                    el_rpc: Some(Url::parse("http://localhost:7545").unwrap()),
                    docker_conductor: Some("op-conductor-0".to_string()),
                    docker_el: Some("base-builder".to_string()),
                    docker_cl: Some("base-builder-cl".to_string()),
                    flashblocks_ws: Some(Url::parse("ws://localhost:7111").unwrap()),
                },
                ConductorNodeConfig {
                    name: "op-conductor-1".to_string(),
                    conductor_rpc: Url::parse("http://localhost:6546").unwrap(),
                    cl_rpc: Url::parse("http://localhost:10549").unwrap(),
                    server_id: "sequencer-1".to_string(),
                    raft_addr: "op-conductor-1:5051".to_string(),
                    el_rpc: Some(Url::parse("http://localhost:10545").unwrap()),
                    docker_conductor: Some("op-conductor-1".to_string()),
                    docker_el: Some("base-sequencer-1".to_string()),
                    docker_cl: Some("base-sequencer-1-cl".to_string()),
                    flashblocks_ws: Some(Url::parse("ws://localhost:10111").unwrap()),
                },
                ConductorNodeConfig {
                    name: "op-conductor-2".to_string(),
                    conductor_rpc: Url::parse("http://localhost:6547").unwrap(),
                    cl_rpc: Url::parse("http://localhost:11549").unwrap(),
                    server_id: "sequencer-2".to_string(),
                    raft_addr: "op-conductor-2:5052".to_string(),
                    el_rpc: Some(Url::parse("http://localhost:11545").unwrap()),
                    docker_conductor: Some("op-conductor-2".to_string()),
                    docker_el: Some("base-sequencer-2".to_string()),
                    docker_cl: Some("base-sequencer-2-cl".to_string()),
                    flashblocks_ws: Some(Url::parse("ws://localhost:11111").unwrap()),
                },
            ]),
            validators: Some(vec![
                ValidatorNodeConfig {
                    name: "base-client".to_string(),
                    binary: Some("/app/base-client + /app/base-consensus".to_string()),
                    cl_rpc: Url::parse("http://localhost:8549").unwrap(),
                    el_rpc: Some(Url::parse("http://localhost:8545").unwrap()),
                    docker_el: Some("base-client".to_string()),
                    docker_cl: Some("base-client-cl".to_string()),
                },
                ValidatorNodeConfig {
                    name: "base-rpc".to_string(),
                    binary: Some("/app/base".to_string()),
                    cl_rpc: Url::parse("http://localhost:8649").unwrap(),
                    el_rpc: Some(Url::parse("http://localhost:8645").unwrap()),
                    docker_el: Some("base-rpc".to_string()),
                    docker_cl: Some("base-rpc".to_string()),
                },
            ]),
            discovery: None,
            proofs: None,
            pods: None,
        }
    }

    /// Fetches the rollup config from the consensus node via the `optimism_rollupConfig` RPC method.
    async fn fetch_rollup_config(consensus_node_url: &Url) -> Result<RollupConfig> {
        let provider =
            ProviderBuilder::new().connect(consensus_node_url.as_str()).await.with_context(
                || format!("Failed to connect to consensus node at {consensus_node_url}"),
            )?;

        let config: RollupConfig =
            provider
                .raw_request("optimism_rollupConfig".into(), ())
                .await
                .with_context(|| "Failed to fetch rollup config from consensus node")?;

        Ok(config)
    }

    /// Load config by name or path
    ///
    /// Resolution order:
    /// 1. Built-in config as base (if name matches "mainnet", "sepolia", or "devnet")
    /// 2. User config at ~/.config/base/networks/<name>.yaml merged on top
    /// 3. Or treat as standalone file path
    ///
    /// For devnet, the `system_config` and `batcher_address` are fetched dynamically
    /// from the consensus node via the `optimism_rollupConfig` RPC method.
    pub async fn load(name_or_path: &str) -> Result<Self> {
        let base_config = match name_or_path {
            "mainnet" => Some(Self::mainnet()),
            "sepolia" => Some(Self::sepolia()),
            "devnet" => Some(Self::load_devnet().await?),
            _ => None,
        };

        if let Some(config_dir) = Self::config_dir() {
            let yaml_path = config_dir.join(format!("{name_or_path}.yaml"));
            let yml_path = config_dir.join(format!("{name_or_path}.yml"));
            let user_config_path = if yaml_path.exists() {
                Some(yaml_path)
            } else if yml_path.exists() {
                Some(yml_path)
            } else {
                None
            };
            if let Some(user_config_path) = user_config_path {
                return base_config.map_or_else(
                    || Self::load_from_file(&user_config_path),
                    |base| Self::load_and_merge(&user_config_path, base),
                );
            }
        }

        if let Some(config) = base_config {
            return Ok(config);
        }

        let path = PathBuf::from(name_or_path);
        if path.exists() {
            return Self::load_from_file(&path);
        }

        anyhow::bail!(
            "Config '{name_or_path}' not found. Expected built-in name (mainnet, sepolia, devnet), \
             user config at ~/.config/base/networks/{name_or_path}.yaml, or a valid file path."
        )
    }

    /// Load devnet config by fetching addresses from the consensus node.
    async fn load_devnet() -> Result<Self> {
        let mut config = Self::devnet_base();

        let consensus_node_url =
            config.consensus_node_rpc.as_ref().expect("devnet should have consensus_node_rpc");

        let rollup_config = Self::fetch_rollup_config(consensus_node_url).await.with_context(
            || "Failed to fetch rollup config from consensus node. Is the devnet running?",
        )?;

        config.system_config = rollup_config.l1_system_config_address;
        config.batcher_address = rollup_config.genesis.system_config.map(|sc| sc.batcher_address);
        config.upgrades = Some(rollup_config.upgrades);

        Ok(config)
    }

    fn load_from_file(path: &PathBuf) -> Result<Self> {
        let contents = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read config file: {}", path.display()))?;

        let config: Self = serde_yaml::from_str(&contents)
            .with_context(|| format!("Failed to parse config file: {}", path.display()))?;

        Ok(config)
    }

    fn load_and_merge(path: &PathBuf, base: Self) -> Result<Self> {
        let contents = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read config file: {}", path.display()))?;

        let overrides: MonitoringConfigOverride = serde_yaml::from_str(&contents)
            .with_context(|| format!("Failed to parse config file: {}", path.display()))?;

        Ok(Self {
            name: overrides.name.unwrap_or(base.name),
            rpc: overrides.rpc.unwrap_or(base.rpc),
            flashblocks_ws: overrides.flashblocks_ws.unwrap_or(base.flashblocks_ws),
            l1_rpc: overrides.l1_rpc.unwrap_or(base.l1_rpc),
            consensus_node_rpc: overrides.consensus_node_rpc.or(base.consensus_node_rpc),
            upgrades: overrides.upgrades.or(base.upgrades),
            system_config: overrides.system_config.unwrap_or(base.system_config),
            batcher_address: overrides.batcher_address.or(base.batcher_address),
            l1_blob_target: overrides.l1_blob_target.unwrap_or(base.l1_blob_target),
            conductors: overrides.conductors.or(base.conductors),
            discovery: overrides.discovery.or(base.discovery),
            validators: overrides.validators.or(base.validators),
            proofs: overrides.proofs.or(base.proofs),
            pods: overrides.pods.or(base.pods),
        })
    }

    fn config_dir() -> Option<PathBuf> {
        dirs::home_dir().map(|h| h.join(".config").join("base").join("networks"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_builtin_configs() {
        let mainnet = MonitoringConfig::load("mainnet").await.unwrap();
        assert_eq!(mainnet.name, "mainnet");
        assert!(mainnet.rpc.as_str().contains("mainnet"));

        let sepolia = MonitoringConfig::load("sepolia").await.unwrap();
        assert_eq!(sepolia.name, "sepolia");
        assert!(sepolia.rpc.as_str().contains("sepolia"));
    }

    #[test]
    fn test_devnet_base_config() {
        // Test the base devnet config structure (without RPC call)
        let devnet = MonitoringConfig::devnet_base();
        assert_eq!(devnet.name, "devnet");
        assert!(devnet.rpc.as_str().contains("localhost"));
        assert_eq!(devnet.rpc.as_str(), "http://localhost:7545/");
        assert_eq!(devnet.flashblocks_ws.as_str(), "ws://localhost:7111/");
        assert_eq!(devnet.l1_rpc.as_str(), "http://localhost:4545/");
        assert!(devnet.consensus_node_rpc.is_some());
        assert_eq!(devnet.consensus_node_rpc.unwrap().as_str(), "http://localhost:7549/");
        let validators = devnet.validators.expect("devnet should include validator/RPC node");
        assert_eq!(validators.len(), 2);
        assert_eq!(validators[0].name, "base-client");
        assert_eq!(validators[0].binary.as_deref(), Some("/app/base-client + /app/base-consensus"));
        assert_eq!(validators[0].cl_rpc.as_str(), "http://localhost:8549/");
        assert_eq!(validators[0].el_rpc.as_ref().unwrap().as_str(), "http://localhost:8545/");
        assert_eq!(validators[0].docker_el.as_deref(), Some("base-client"));
        assert_eq!(validators[0].docker_cl.as_deref(), Some("base-client-cl"));
        assert_eq!(validators[1].name, "base-rpc");
        assert_eq!(validators[1].binary.as_deref(), Some("/app/base"));
        assert_eq!(validators[1].cl_rpc.as_str(), "http://localhost:8649/");
        assert_eq!(validators[1].el_rpc.as_ref().unwrap().as_str(), "http://localhost:8645/");
        assert_eq!(validators[1].docker_el.as_deref(), Some("base-rpc"));
        assert_eq!(validators[1].docker_cl.as_deref(), Some("base-rpc"));
    }

    #[tokio::test]
    async fn test_unknown_config() {
        let result = MonitoringConfig::load("nonexistent").await;
        assert!(result.is_err());
    }

    #[test]
    fn conductor_source_prefers_static_nodes() {
        let mut config = MonitoringConfig::devnet_base();
        let cli_url = Url::parse("http://127.0.0.1:5545").unwrap();

        let Some(ConductorSource::Static(nodes)) = config.conductor_source(Some(cli_url)) else {
            panic!("expected static conductor source");
        };

        assert_eq!(nodes.len(), 3);
        assert_eq!(nodes[0].name, "op-conductor-0");
        config.conductors = None;
        assert!(config.conductor_source(None).is_none());
    }

    #[test]
    fn conductor_source_uses_cli_flag_without_static_nodes() {
        let mut config = MonitoringConfig::mainnet();
        let cli_url = Url::parse("http://127.0.0.1:5545").unwrap();

        let Some(ConductorSource::Discover { bootstrap, ports }) =
            config.conductor_source(Some(cli_url.clone()))
        else {
            panic!("expected discovered conductor source");
        };

        assert_eq!(bootstrap, cli_url);
        assert_eq!(ports.conductor_rpc, 5545);
        config.discovery = None;
        assert!(config.conductor_source(None).is_none());
    }

    #[test]
    fn conductor_source_uses_config_discovery_bootstrap() {
        let mut config = MonitoringConfig::mainnet();
        let bootstrap = Url::parse("http://10.0.0.1:5545").unwrap();
        config.discovery.as_mut().unwrap().bootstrap_rpc = Some(bootstrap.clone());

        let Some(ConductorSource::Discover { bootstrap: resolved, .. }) =
            config.conductor_source(None)
        else {
            panic!("expected discovered conductor source");
        };

        assert_eq!(resolved, bootstrap);
    }
}
