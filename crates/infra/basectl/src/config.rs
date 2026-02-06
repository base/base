use std::path::PathBuf;

use alloy_primitives::Address;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use url::Url;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChainConfig {
    pub name: String,
    pub rpc: Url,
    pub flashblocks_ws: Url,
    pub l1_rpc: Url,
    pub system_config: Address,
}

impl ChainConfig {
    /// Built-in mainnet configuration
    pub fn mainnet() -> Self {
        Self {
            name: "mainnet".to_string(),
            rpc: Url::parse("https://mainnet.base.org").unwrap(),
            flashblocks_ws: Url::parse("wss://mainnet.flashblocks.base.org/ws").unwrap(),
            l1_rpc: Url::parse("https://ethereum-rpc.publicnode.com").unwrap(),
            system_config: "0x73a79Fab69143498Ed3712e519A88a918e1f4072".parse().unwrap(),
        }
    }

    /// Built-in sepolia configuration
    pub fn sepolia() -> Self {
        Self {
            name: "sepolia".to_string(),
            rpc: Url::parse("https://sepolia.base.org").unwrap(),
            flashblocks_ws: Url::parse("wss://sepolia.flashblocks.base.org/ws").unwrap(),
            l1_rpc: Url::parse("https://ethereum-sepolia-rpc.publicnode.com").unwrap(),
            system_config: "0xf272670eb55e895584501d564AfEB048bEd26194".parse().unwrap(),
        }
    }

    /// Load config by name or path
    ///
    /// Resolution order:
    /// 1. Built-in configs ("mainnet", "sepolia")
    /// 2. User config at ~/.base/config/<name>.yaml
    /// 3. Treat as file path
    pub fn load(name_or_path: &str) -> Result<Self> {
        // Check built-in configs first
        match name_or_path {
            "mainnet" => return Ok(Self::mainnet()),
            "sepolia" => return Ok(Self::sepolia()),
            _ => {}
        }

        // Check user config directory
        if let Some(config_dir) = Self::config_dir() {
            let user_config_path = config_dir.join(format!("{name_or_path}.yaml"));
            if user_config_path.exists() {
                return Self::load_from_file(&user_config_path);
            }
        }

        // Treat as file path
        let path = PathBuf::from(name_or_path);
        if path.exists() {
            return Self::load_from_file(&path);
        }

        anyhow::bail!(
            "Config '{name_or_path}' not found. Expected built-in name (mainnet, sepolia), \
             user config at ~/.base/config/{name_or_path}.yaml, or a valid file path."
        )
    }

    fn load_from_file(path: &PathBuf) -> Result<Self> {
        let contents = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read config file: {}", path.display()))?;

        let config: Self = serde_yaml::from_str(&contents)
            .with_context(|| format!("Failed to parse config file: {}", path.display()))?;

        Ok(config)
    }

    fn config_dir() -> Option<PathBuf> {
        dirs::home_dir().map(|h| h.join(".base").join("config"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_builtin_configs() {
        let mainnet = ChainConfig::load("mainnet").unwrap();
        assert_eq!(mainnet.name, "mainnet");
        assert!(mainnet.rpc.as_str().contains("mainnet"));

        let sepolia = ChainConfig::load("sepolia").unwrap();
        assert_eq!(sepolia.name, "sepolia");
        assert!(sepolia.rpc.as_str().contains("sepolia"));
    }

    #[test]
    fn test_unknown_config() {
        let result = ChainConfig::load("nonexistent");
        assert!(result.is_err());
    }
}
