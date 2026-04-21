use std::path::PathBuf;

use alloy_primitives::Address;
use alloy_provider::{Provider, ProviderBuilder};
use anyhow::{Context, Result};
use base_consensus_genesis::RollupConfig;
use base_consensus_registry::Registry;
use serde::{Deserialize, Serialize};
use url::Url;

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
    /// Optional OP-Node JSON-RPC endpoint URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub op_node_rpc: Option<Url>,
    /// L1 `SystemConfig` contract address.
    pub system_config: Address,
    /// L1 batcher address for blob attribution.
    ///
    /// This is the current live batcher address, not necessarily the genesis
    /// batcher. It may differ from the value in `base-consensus-registry` if
    /// the batcher was updated via a `SystemConfig` transaction after genesis.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batcher_address: Option<Address>,
    /// Expected number of blobs per L1 block target.
    #[serde(default = "default_blob_target")]
    pub l1_blob_target: u64,
    /// HA conductor cluster nodes, if this chain runs an op-conductor setup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conductors: Option<Vec<ConductorNodeConfig>>,
    /// Validator (non-sequencing) nodes to monitor alongside the conductor cluster.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub validators: Option<Vec<ValidatorNodeConfig>>,
    /// Proof system monitoring configuration (dispute games, anchor state).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proofs: Option<ProofsConfig>,
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
    op_node_rpc: Option<Url>,
    #[serde(default)]
    system_config: Option<Address>,
    #[serde(default)]
    batcher_address: Option<Address>,
    l1_blob_target: Option<u64>,
    conductors: Option<Vec<ConductorNodeConfig>>,
    validators: Option<Vec<ValidatorNodeConfig>>,
    proofs: Option<ProofsConfig>,
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
        let rollup =
            Registry::rollup_config(8453).expect("Base mainnet config missing from registry");
        Self {
            name: "mainnet".to_string(),
            rpc: Url::parse("https://mainnet.base.org").unwrap(),
            flashblocks_ws: Url::parse("wss://mainnet.flashblocks.base.org/ws").unwrap(),
            l1_rpc: Url::parse("https://ethereum-rpc.publicnode.com").unwrap(),
            op_node_rpc: None,
            system_config: rollup.l1_system_config_address,
            batcher_address: Some("0x5050F69a9786F081509234F1a7F4684b5E5b76C9".parse().unwrap()),
            l1_blob_target: 14,
            conductors: None,
            validators: None,
            proofs: None,
        }
    }

    /// Returns the default Base Sepolia configuration.
    pub fn sepolia() -> Self {
        let rollup =
            Registry::rollup_config(84532).expect("Base Sepolia config missing from registry");
        Self {
            name: "sepolia".to_string(),
            rpc: Url::parse("https://sepolia.base.org").unwrap(),
            flashblocks_ws: Url::parse("wss://sepolia.flashblocks.base.org/ws").unwrap(),
            l1_rpc: Url::parse("https://ethereum-sepolia-rpc.publicnode.com").unwrap(),
            op_node_rpc: None,
            system_config: rollup.l1_system_config_address,
            batcher_address: Some("0xfc56E7272EEBBBA5bC6c544e159483C4a38f8bA3".parse().unwrap()),
            l1_blob_target: 14,
            conductors: None,
            validators: None,
            proofs: None,
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
            op_node_rpc: Some(Url::parse("http://localhost:7549").unwrap()),
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
            validators: Some(vec![ValidatorNodeConfig {
                name: "base-client".to_string(),
                cl_rpc: Url::parse("http://localhost:8549").unwrap(),
                el_rpc: Some(Url::parse("http://localhost:8545").unwrap()),
                docker_el: Some("base-client".to_string()),
                docker_cl: Some("base-client-cl".to_string()),
            }]),
            proofs: None,
        }
    }

    /// Fetches the rollup config from the consensus node via the `optimism_rollupConfig` RPC method.
    async fn fetch_rollup_config(op_node_url: &Url) -> Result<RollupConfig> {
        let provider = ProviderBuilder::new()
            .connect(op_node_url.as_str())
            .await
            .with_context(|| format!("Failed to connect to consensus node at {op_node_url}"))?;

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

        let op_node_url = config.op_node_rpc.as_ref().expect("devnet should have op_node_rpc");

        let rollup_config = Self::fetch_rollup_config(op_node_url).await.with_context(
            || "Failed to fetch rollup config from consensus node. Is the devnet running?",
        )?;

        config.system_config = rollup_config.l1_system_config_address;
        config.batcher_address = rollup_config.genesis.system_config.map(|sc| sc.batcher_address);

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
            op_node_rpc: overrides.op_node_rpc.or(base.op_node_rpc),
            system_config: overrides.system_config.unwrap_or(base.system_config),
            batcher_address: overrides.batcher_address.or(base.batcher_address),
            l1_blob_target: overrides.l1_blob_target.unwrap_or(base.l1_blob_target),
            conductors: overrides.conductors.or(base.conductors),
            validators: overrides.validators.or(base.validators),
            proofs: overrides.proofs.or(base.proofs),
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
        assert!(devnet.op_node_rpc.is_some());
        assert_eq!(devnet.op_node_rpc.unwrap().as_str(), "http://localhost:7549/");
    }

    #[tokio::test]
    async fn test_unknown_config() {
        let result = MonitoringConfig::load("nonexistent").await;
        assert!(result.is_err());
    }
}
