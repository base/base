//! Deployment artifact types (genesis, rollup config, addresses).

use std::path::Path;

use eyre::{Result, WrapErr};
use serde_json::Value;

const L2_GENESIS_FILE: &str = "genesis.json";
const ROLLUP_CONFIG_FILE: &str = "rollup.json";
const L1_ADDRESSES_FILE: &str = "l1-addresses.json";

/// Artifacts emitted by op-deployer.
#[derive(Debug, Clone)]
pub struct DeploymentArtifacts {
    /// L2 genesis configuration.
    pub l2_genesis: Value,
    /// Rollup configuration.
    pub rollup_config: Value,
    /// L1 contract addresses for the deployment.
    pub l1_addresses: Value,
}

impl DeploymentArtifacts {
    /// Returns true when all expected artifacts are present.
    pub fn exists_in(dir: impl AsRef<Path>) -> bool {
        let dir = dir.as_ref();
        dir.join(L2_GENESIS_FILE).exists()
            && dir.join(ROLLUP_CONFIG_FILE).exists()
            && dir.join(L1_ADDRESSES_FILE).exists()
    }

    /// Loads deployment artifacts from the output directory.
    pub fn load_from_dir(dir: impl AsRef<Path>) -> Result<Self> {
        let dir = dir.as_ref();
        let l2_genesis = read_json(&dir.join(L2_GENESIS_FILE))?;
        let rollup_config = read_json(&dir.join(ROLLUP_CONFIG_FILE))?;
        let l1_addresses = read_json(&dir.join(L1_ADDRESSES_FILE))?;

        Ok(Self { l2_genesis, rollup_config, l1_addresses })
    }
}

fn read_json(path: &Path) -> Result<Value> {
    let contents = std::fs::read_to_string(path)
        .wrap_err_with(|| format!("Failed to read artifact at {}", path.display()))?;
    let value = serde_json::from_str(&contents)
        .wrap_err_with(|| format!("Failed to parse JSON at {}", path.display()))?;
    Ok(value)
}
