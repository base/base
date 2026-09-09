use std::{path::PathBuf, process::Command};

use base_genesis::{ContractArtifacts, GenesisConfig, GenesisOutput};
use eyre::{Result, WrapErr, ensure};

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
    pub fn generate_genesis(&self) -> Result<GenesisOutput> {
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
        let output = GenesisOutput::new(&self.output_dir);
        output.generate(&self.config, &ContractArtifacts::load(&path)?)?;
        Ok(output)
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
        let output = setup.generate_genesis()?;
        assert!(output.l1.join("cl/genesis.ssz").is_file());
        let l1_genesis: serde_json::Value =
            serde_json::from_slice(&std::fs::read(output.l1.join("el/genesis.json"))?)?;
        let genesis: serde_json::Value =
            serde_json::from_slice(&std::fs::read(output.l2.join("genesis.json"))?)?;
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
