use std::{path::PathBuf, process::Command};

use base_genesis::{ContractArtifacts, GenesisConfig, GenesisOutput};
use eyre::{Result, WrapErr, ensure};

/// Output of the L1 genesis generation.
#[derive(Debug, Clone)]
pub struct L1GenesisOutput {
    output_dir: PathBuf,
}

impl L1GenesisOutput {
    /// Returns the path to the EL genesis JSON file.
    pub fn el_genesis_path(&self) -> PathBuf {
        self.output_dir.join("el/genesis.json")
    }

    /// Returns the path to the CL genesis SSZ file.
    pub fn cl_genesis_ssz_path(&self) -> PathBuf {
        self.output_dir.join("cl/genesis.ssz")
    }

    /// Returns the path to the CL configuration YAML file.
    pub fn cl_config_path(&self) -> PathBuf {
        self.output_dir.join("cl/config.yaml")
    }

    /// Returns the path to the JWT secret file.
    pub fn jwt_path(&self) -> PathBuf {
        self.output_dir.join("jwt.hex")
    }

    /// Returns the path to the validator data directory.
    pub fn validator_data_path(&self) -> PathBuf {
        self.output_dir.join("cl/validator_data")
    }

    /// Returns the path to the testnet directory.
    pub fn testnet_dir(&self) -> PathBuf {
        self.output_dir.join("cl")
    }

    /// Reads and returns the JWT secret.
    pub fn read_jwt_secret(&self) -> Result<String> {
        std::fs::read_to_string(self.jwt_path()).wrap_err("Failed to read jwt.hex")
    }

    /// Reads and returns the EL genesis JSON content.
    pub fn read_el_genesis(&self) -> Result<String> {
        std::fs::read_to_string(self.el_genesis_path()).wrap_err("Failed to read el genesis")
    }
}

/// Output of the L2 contract deployment.
#[derive(Debug, Clone)]
pub struct L2DeploymentOutput {
    output_dir: PathBuf,
}

impl L2DeploymentOutput {
    /// Returns the path to the L2 genesis JSON file.
    pub fn genesis_path(&self) -> PathBuf {
        self.output_dir.join("l2/genesis.json")
    }

    /// Returns the path to the rollup configuration JSON file.
    pub fn rollup_config_path(&self) -> PathBuf {
        self.output_dir.join("l2/rollup.json")
    }

    /// Returns the path to the L1 addresses JSON file.
    pub fn l1_addresses_path(&self) -> PathBuf {
        self.output_dir.join("l2/l1-addresses.json")
    }

    /// Reads and returns the L2 genesis JSON content.
    pub fn read_genesis(&self) -> Result<String> {
        std::fs::read_to_string(self.genesis_path()).wrap_err("Failed to read l2 genesis")
    }

    /// Reads and returns the rollup configuration JSON content.
    pub fn read_rollup_config(&self) -> Result<String> {
        std::fs::read_to_string(self.rollup_config_path()).wrap_err("Failed to read rollup config")
    }
}

/// In-process generation of both chains for system tests.
#[derive(Debug, Clone)]
pub struct GenesisSetup {
    output_dir: PathBuf,
    /// Native generator inputs; callers may customize roles, time, and fork settings directly.
    pub config: GenesisConfig,
}

impl GenesisSetup {
    /// Creates a new genesis setup with the given output directory.
    pub fn new(output_dir: impl Into<PathBuf>) -> Self {
        Self {
            output_dir: output_dir.into(),
            config: GenesisConfig {
                slot_duration: 1,
                preinstall_upgrade_signal: false,
                ..Default::default()
            },
        }
    }

    /// Generates both chains before starting L1, with deployed contracts in genesis.
    pub fn generate_genesis(&self) -> Result<(L1GenesisOutput, L2DeploymentOutput)> {
        let path = if let Some(path) = std::env::var_os("BASE_GENESIS_ARTIFACTS") {
            PathBuf::from(path)
        } else {
            // Provision in the test harness, never in the offline generator.
            // The helper serializes concurrent nextest processes and verifies cached contents.
            let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
            let status = Command::new("python3")
                .arg(root.join("etc/scripts/devnet/contracts.py"))
                .current_dir(&root)
                .status()
                .wrap_err("preparing system-test contracts; run `just contracts` or set BASE_GENESIS_ARTIFACTS")?;
            ensure!(
                status.success(),
                "system-test contract preparation failed; run `just contracts` or set BASE_GENESIS_ARTIFACTS"
            );
            root.join(".contracts/artifacts")
        };
        GenesisOutput::new(&self.output_dir)
            .generate(&self.config, &ContractArtifacts::load(&path)?)?;
        Ok((
            L1GenesisOutput { output_dir: self.output_dir.clone() },
            L2DeploymentOutput { output_dir: self.output_dir.clone() },
        ))
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn generates_fresh_chains_through_the_system_test_entrypoint() -> Result<()> {
        let root = TempDir::new()?;
        let mut setup = GenesisSetup::new(root.path());
        setup.config.l1_chain_id = 1338;
        setup.config.l2_chain_id = 84538454;
        setup.config.upgrades.insert("isthmus".into(), 10);
        let (l1, l2) = setup.generate_genesis()?;
        assert!(l1.cl_genesis_ssz_path().is_file());
        let l1_genesis: serde_json::Value = serde_json::from_str(&l1.read_el_genesis()?)?;
        let genesis: serde_json::Value = serde_json::from_str(&l2.read_genesis()?)?;
        assert_eq!(l1_genesis["config"]["chainId"], 1338);
        assert_eq!(genesis["config"]["chainId"], 84538454);
        let activation = genesis["timestamp"].as_str().unwrap();
        let timestamp = u64::from_str_radix(activation.trim_start_matches("0x"), 16)?;
        assert_eq!(genesis["config"]["isthmusTime"], timestamp + 20);
        assert_eq!(genesis["config"]["jovianTime"], timestamp + 20);
        assert!(!root.path().join("l2/upgrade-signal.env").exists());
        Ok(())
    }
}
