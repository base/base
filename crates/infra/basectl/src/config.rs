use std::path::PathBuf;

use alloy_primitives::Address;
use alloy_provider::{Provider, ProviderBuilder};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use url::Url;

/// Configuration for a chain monitored by basectl.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChainConfig {
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
    #[serde(with = "address_serde")]
    pub system_config: Address,
    /// L1 batcher address for blob attribution.
    #[serde(default, skip_serializing_if = "Option::is_none", with = "option_address_serde")]
    pub batcher_address: Option<Address>,
    /// Expected number of blobs per L1 block target.
    #[serde(default = "default_blob_target")]
    pub l1_blob_target: u64,
}

const fn default_blob_target() -> u64 {
    14
}

#[derive(Debug, Clone, Deserialize, Default)]
struct ChainConfigOverride {
    name: Option<String>,
    rpc: Option<Url>,
    flashblocks_ws: Option<Url>,
    l1_rpc: Option<Url>,
    op_node_rpc: Option<Url>,
    #[serde(default, with = "option_address_serde")]
    system_config: Option<Address>,
    #[serde(default, with = "option_address_serde")]
    batcher_address: Option<Address>,
    l1_blob_target: Option<u64>,
}

mod address_serde {
    use std::str::FromStr;

    use alloy_primitives::Address;
    use serde::{self, Deserialize, Deserializer, Serializer};

    pub(super) fn serialize<S>(address: &Address, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&format!("{address:#x}"))
    }

    pub(super) fn deserialize<'de, D>(deserializer: D) -> Result<Address, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Address::from_str(&s).map_err(serde::de::Error::custom)
    }
}

mod option_address_serde {
    use std::str::FromStr;

    use alloy_primitives::Address;
    use serde::{self, Deserialize, Deserializer, Serializer};

    pub(super) fn serialize<S>(address: &Option<Address>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match address {
            Some(addr) => serializer.serialize_str(&format!("{addr:#x}")),
            None => serializer.serialize_none(),
        }
    }

    pub(super) fn deserialize<'de, D>(deserializer: D) -> Result<Option<Address>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let opt: Option<String> = Option::deserialize(deserializer)?;
        opt.map_or_else(
            || Ok(None),
            |s| Address::from_str(&s).map(Some).map_err(serde::de::Error::custom),
        )
    }
}

/// Response from the `optimism_rollupConfig` RPC method.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct RollupConfig {
    /// Genesis configuration block.
    pub genesis: GenesisConfig,
    /// L1 `SystemConfig` contract address.
    pub l1_system_config_address: Address,
}

/// Genesis configuration from rollup config.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct GenesisConfig {
    /// System configuration at genesis.
    pub system_config: GenesisSystemConfig,
}

/// System config within genesis.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct GenesisSystemConfig {
    /// Batcher address configured at genesis.
    #[serde(rename = "batcherAddr")]
    pub batcher_addr: Address,
}

impl ChainConfig {
    /// Returns the default Base mainnet configuration.
    pub(crate) fn mainnet() -> Self {
        Self {
            name: "mainnet".to_string(),
            rpc: Url::parse("https://mainnet.base.org").unwrap(),
            flashblocks_ws: Url::parse("wss://mainnet.flashblocks.base.org/ws").unwrap(),
            l1_rpc: Url::parse("https://ethereum-rpc.publicnode.com").unwrap(),
            op_node_rpc: None,
            system_config: "0x73a79Fab69143498Ed3712e519A88a918e1f4072".parse().unwrap(),
            batcher_address: Some("0x5050F69a9786F081509234F1a7F4684b5E5b76C9".parse().unwrap()),
            l1_blob_target: 14,
        }
    }

    /// Returns the default Base Sepolia configuration.
    pub(crate) fn sepolia() -> Self {
        Self {
            name: "sepolia".to_string(),
            rpc: Url::parse("https://sepolia.base.org").unwrap(),
            flashblocks_ws: Url::parse("wss://sepolia.flashblocks.base.org/ws").unwrap(),
            l1_rpc: Url::parse("https://ethereum-sepolia-rpc.publicnode.com").unwrap(),
            op_node_rpc: None,
            system_config: "0xf272670eb55e895584501d564AfEB048bEd26194".parse().unwrap(),
            batcher_address: Some("0xfc56E7272EEBBBA5bC6c544e159483C4a38f8bA3".parse().unwrap()),
            l1_blob_target: 14,
        }
    }

    /// Returns a devnet configuration for local development.
    ///
    /// The devnet addresses are fetched dynamically from the op-node via the
    /// `optimism_rollupConfig` RPC method since they are regenerated each time
    /// the devnet is started.
    ///
    /// Use `load("devnet")` to get a fully configured devnet with addresses
    /// fetched from the running op-node.
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
        }
    }

    /// Fetches the rollup config from the op-node via the `optimism_rollupConfig` RPC method.
    async fn fetch_rollup_config(op_node_url: &Url) -> Result<RollupConfig> {
        let provider = ProviderBuilder::new()
            .connect(op_node_url.as_str())
            .await
            .with_context(|| format!("Failed to connect to op-node at {op_node_url}"))?;

        let config: RollupConfig = provider
            .raw_request("optimism_rollupConfig".into(), ())
            .await
            .with_context(|| "Failed to fetch rollup config from op-node")?;

        Ok(config)
    }

    /// Load config by name or path
    ///
    /// Resolution order:
    /// 1. Built-in config as base (if name matches "mainnet", "sepolia", or "devnet")
    /// 2. User config at ~/.base/config/<name>.yaml merged on top
    /// 3. Or treat as standalone file path
    ///
    /// For devnet, the `system_config` and `batcher_address` are fetched dynamically
    /// from the op-node via the `optimism_rollupConfig` RPC method.
    pub async fn load(name_or_path: &str) -> Result<Self> {
        let base_config = match name_or_path {
            "mainnet" => Some(Self::mainnet()),
            "sepolia" => Some(Self::sepolia()),
            "devnet" => Some(Self::load_devnet().await?),
            _ => None,
        };

        if let Some(config_dir) = Self::config_dir() {
            let user_config_path = config_dir.join(format!("{name_or_path}.yaml"));
            if user_config_path.exists() {
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
             user config at ~/.base/config/{name_or_path}.yaml, or a valid file path."
        )
    }

    /// Load devnet config by fetching addresses from the op-node.
    async fn load_devnet() -> Result<Self> {
        let mut config = Self::devnet_base();

        let op_node_url = config.op_node_rpc.as_ref().expect("devnet should have op_node_rpc");

        let rollup_config = Self::fetch_rollup_config(op_node_url).await.with_context(
            || "Failed to fetch rollup config from op-node. Is the devnet running?",
        )?;

        config.system_config = rollup_config.l1_system_config_address;
        config.batcher_address = Some(rollup_config.genesis.system_config.batcher_addr);

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

        let overrides: ChainConfigOverride = serde_yaml::from_str(&contents)
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
        })
    }

    fn config_dir() -> Option<PathBuf> {
        dirs::home_dir().map(|h| h.join(".base").join("config"))
    }
}

#[cfg(test)]
mod tests {
    use super::ChainConfig;

    #[tokio::test]
    async fn test_builtin_configs() {
        let mainnet = ChainConfig::load("mainnet").await.unwrap();
        assert_eq!(mainnet.name, "mainnet");
        assert!(mainnet.rpc.as_str().contains("mainnet"));

        let sepolia = ChainConfig::load("sepolia").await.unwrap();
        assert_eq!(sepolia.name, "sepolia");
        assert!(sepolia.rpc.as_str().contains("sepolia"));
    }

    #[test]
    fn test_devnet_base_config() {
        // Test the base devnet config structure (without RPC call)
        let devnet = ChainConfig::devnet_base();
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
        let result = ChainConfig::load("nonexistent").await;
        assert!(result.is_err());
    }
}
