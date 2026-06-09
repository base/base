//! Rollup config artifact patching.

use std::{
    fs,
    path::{Path, PathBuf},
};

use alloy_genesis::Genesis;
use alloy_primitives::B256;
use base_execution_chainspec::BaseChainSpec;
use eyre::{OptionExt, Result, WrapErr};
use reth_chainspec::EthChainSpec;
use serde_json::Value;

/// Patches generated rollup configs so they match the final L2 genesis file.
#[derive(Debug, Clone, Copy)]
pub struct RollupConfigPatcher;

impl RollupConfigPatcher {
    /// L2 genesis artifact filename.
    pub const GENESIS_FILE: &'static str = "genesis.json";
    /// Consensus rollup config artifact filename.
    pub const ROLLUP_FILE: &'static str = "rollup.json";
    /// Conductor-compatible rollup config artifact filename.
    pub const CONDUCTOR_ROLLUP_FILE: &'static str = "rollup-conductor.json";

    /// Patches all rollup config artifacts in a deployment artifact directory.
    pub fn patch_dir(dir: impl AsRef<Path>) -> Result<B256> {
        let dir = dir.as_ref();
        let genesis_path = dir.join(Self::GENESIS_FILE);
        let rollup_path = dir.join(Self::ROLLUP_FILE);
        let hash = Self::patch_rollup_file(&genesis_path, &rollup_path)?;

        let conductor_rollup_path = dir.join(Self::CONDUCTOR_ROLLUP_FILE);
        if conductor_rollup_path.exists() {
            Self::patch_rollup_file(&genesis_path, &conductor_rollup_path)?;
        }

        Ok(hash)
    }

    /// Patches one rollup config file to reference the final L2 genesis hash.
    pub fn patch_rollup_file(
        genesis_path: impl AsRef<Path>,
        rollup_path: impl AsRef<Path>,
    ) -> Result<B256> {
        let hash = Self::genesis_hash(genesis_path)?;
        let rollup_path = rollup_path.as_ref();
        let mut rollup = Self::read_json(rollup_path)?;
        let l2_hash = rollup
            .pointer_mut("/genesis/l2/hash")
            .ok_or_eyre("rollup config missing genesis.l2.hash")?;
        *l2_hash = Value::String(format!("{hash:#x}"));
        Self::write_json(rollup_path, &rollup)?;
        Ok(hash)
    }

    /// Computes the Base genesis block hash for a genesis JSON file.
    pub fn genesis_hash(genesis_path: impl AsRef<Path>) -> Result<B256> {
        let genesis_path = genesis_path.as_ref();
        let contents = fs::read_to_string(genesis_path)
            .wrap_err_with(|| format!("Failed to read genesis at {}", genesis_path.display()))?;
        let genesis = serde_json::from_str::<Genesis>(&contents)
            .wrap_err_with(|| format!("Failed to parse genesis at {}", genesis_path.display()))?;
        let chain_spec =
            BaseChainSpec::try_from_genesis(genesis).wrap_err("Failed to build chain spec")?;
        Ok(chain_spec.genesis_hash())
    }

    /// Reads a JSON value from disk.
    pub fn read_json(path: impl Into<PathBuf>) -> Result<Value> {
        let path = path.into();
        let contents = fs::read_to_string(&path)
            .wrap_err_with(|| format!("Failed to read JSON at {}", path.display()))?;
        serde_json::from_str(&contents)
            .wrap_err_with(|| format!("Failed to parse JSON at {}", path.display()))
    }

    /// Writes a JSON value to disk.
    pub fn write_json(path: impl AsRef<Path>, value: &Value) -> Result<()> {
        let path = path.as_ref();
        let contents = serde_json::to_vec_pretty(value)
            .wrap_err_with(|| format!("Failed to encode JSON for {}", path.display()))?;
        fs::write(path, contents)
            .wrap_err_with(|| format!("Failed to write JSON at {}", path.display()))
    }
}
