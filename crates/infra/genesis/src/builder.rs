//! Rust assembly stages for the offline genesis workflow.

use std::{collections::BTreeMap, fs, path::PathBuf};

use alloy_primitives::Address;
use base_common_genesis::{BaseUpgrade, UpgradeConfig};
use eyre::{Result, ensure};
use reth_chainspec::ChainSpec;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{
    BeaconGenesis, ExecutionGenesis, GenesisCommand, GenesisConfig, GenesisForge, GenesisOutput,
    GenesisStage,
};

/// Resolved inputs shared by the Forge workflow and Rust assembly stages.
#[derive(Debug, Serialize, Deserialize)]
pub struct GenesisPlan {
    /// Resolved chain settings.
    pub config: GenesisConfig,
    /// Digest of the verified contract manifest.
    pub contracts: String,
    /// Auxiliary devnet settings.
    pub settings: BTreeMap<String, String>,
    /// Prepared project directory.
    pub project: PathBuf,
    /// Genesis output root.
    pub output: PathBuf,
    /// Whether a complete matching genesis already exists.
    pub complete: bool,
}

/// Computes execution roots and assembles execution and beacon genesis files.
#[derive(Debug)]
pub struct GenesisBuilder;

impl GenesisBuilder {
    /// Execute one Rust-only stage. `just genesis` coordinates the complete workflow.
    pub fn generate(command: GenesisCommand) -> Result<()> {
        Self::generate_with_upgrade_blocks(command, &[])
    }

    /// Execute one Rust-only stage with development upgrade activation blocks.
    pub fn generate_with_upgrade_blocks(
        command: GenesisCommand,
        upgrade_blocks: &[(BaseUpgrade, u64)],
    ) -> Result<()> {
        let work = command.work_dir.ok_or_else(|| {
            eyre::eyre!(
                "use `just genesis` for the full workflow, or supply --work-dir and --stage"
            )
        })?;
        fs::create_dir_all(&work)?;
        let work = work.canonicalize()?;
        if matches!(command.stage, GenesisStage::Prepare) {
            fs::create_dir_all(&command.output_dir)?;
            let output = command.output_dir.canonicalize()?;
            let forge = GenesisForge::open(&command.artifacts_dir)?;
            let settings = GenesisOutput::settings()?;
            let complete = output.join(".setup-complete");
            let mut config = command.config;
            let existing = if complete.exists() {
                let existing: GenesisOutput = serde_json::from_slice(&fs::read(&complete)?)?;
                config.timestamp = config.timestamp.or(existing.config.timestamp);
                config.salt = config.salt.or(existing.config.salt);
                Some(existing)
            } else {
                None
            };
            let mut config = config.resolve()?;
            config.upgrades = Self::upgrades(&config, upgrade_blocks)?;
            if let Some(existing) = &existing {
                existing.validate(&config, &forge.manifest_digest, &settings)?;
                ensure!(
                    existing.files.contains_key(&output.join("l2/genesis.json"))
                        && existing.files.contains_key(&output.join("genesis_timestamp")),
                    "existing genesis uses different output directories"
                );
            }
            let mut deploy = ExecutionGenesis::deploy_config(&config)?;
            deploy["addressesPath"] = json!(work.join("l1-preview/addresses.json"));
            GenesisOutput::write(work.join("input.json"), &serde_json::to_vec(&deploy)?)?;
            let plan = GenesisPlan {
                config,
                contracts: forge.manifest_digest,
                settings,
                project: forge.project,
                output,
                complete: existing.is_some(),
            };
            GenesisOutput::write(work.join("plan.json"), &serde_json::to_vec(&plan)?)?;
            return Ok(());
        }
        let plan: GenesisPlan = serde_json::from_slice(&fs::read(work.join("plan.json"))?)?;
        ensure!(!plan.complete, "genesis is already complete");
        let config = plan.config;
        let l2_spec = ExecutionGenesis::l2(&config, GenesisForge::allocs(&work.join("l2"))?)?;
        if matches!(command.stage, GenesisStage::Anchor) {
            let input = work.join("input.json");
            let mut deploy: serde_json::Value = serde_json::from_slice(&fs::read(&input)?)?;
            deploy["multiproofGenesisOutputRoot"] = json!(l2_spec.genesis_output_root());
            GenesisOutput::write(input, &serde_json::to_vec(&deploy)?)?;
            return Ok(());
        }
        let forge = GenesisForge::open(
            plan.project.parent().ok_or_else(|| eyre::eyre!("missing bundle directory"))?,
        )?;
        ensure!(
            forge.manifest_digest == plan.contracts,
            "contract bundle changed during generation"
        );
        let output = plan.output;
        let settings = plan.settings;
        let final_l1 = work.join("l1-final");
        let addresses: BTreeMap<String, Address> =
            serde_json::from_slice(&fs::read(work.join("l1-preview/addresses.json"))?)?;
        let final_addresses: BTreeMap<String, Address> =
            serde_json::from_slice(&fs::read(final_l1.join("addresses.json"))?)?;
        ensure!(
            addresses == final_addresses,
            "L1 deployment addresses changed when setting the final L2 anchor"
        );
        let l1_genesis =
            ExecutionGenesis::l1(&config, GenesisForge::allocs(&work.join("l1-final"))?)?;
        let l1_spec = ChainSpec::from(l1_genesis.clone());
        let rollup = ExecutionGenesis::rollup(&config, &l1_spec, &l2_spec, &addresses)?;
        let staging = tempfile::Builder::new().prefix(".genesis-").tempdir_in(&output)?;
        for (path, value) in [
            ("el/genesis.json", serde_json::to_value(&l1_genesis)?),
            ("el/chain-config.json", serde_json::to_value(&l1_genesis.config)?),
            ("l2/genesis.json", serde_json::to_value(&l2_spec.genesis)?),
            ("l2/rollup.json", serde_json::to_value(&rollup)?),
            ("l2/l1-addresses.json", serde_json::to_value(&addresses)?),
        ] {
            GenesisOutput::write(staging.path().join(path), &serde_json::to_vec_pretty(&value)?)?;
        }
        let mut conductor = serde_json::to_value(&rollup)?;
        conductor
            .as_object_mut()
            .ok_or_else(|| eyre::eyre!("invalid rollup config"))?
            .remove("base");
        GenesisOutput::write(
            staging.path().join("l2/rollup-conductor.json"),
            &serde_json::to_vec_pretty(&conductor)?,
        )?;
        GenesisOutput::write_keys(staging.path(), &settings)?;
        if config.upgrade_signal {
            let environment = format!(
                "BASE_NODE_UPGRADE_SIGNAL_CONTRACT={}\nBASE_NODE_UPGRADE_SIGNAL_L1_RPC=http://l1-el:{}\nBASE_NODE_UPGRADE_SIGNAL_MODE={}\nBASE_NODE_UPGRADE_SIGNAL_L1_BLOCK_TAG={}\n",
                addresses["ProtocolVersionsProxy"],
                settings["L1_HTTP_PORT"],
                settings["UPGRADE_SIGNAL_MODE"],
                settings["UPGRADE_SIGNAL_L1_BLOCK_TAG"]
            );
            GenesisOutput::write(
                staging.path().join("l2/upgrade-signal.env"),
                environment.as_bytes(),
            )?;
        }
        GenesisOutput::write(
            staging.path().join("genesis_timestamp"),
            format!("{}\n", config.timestamp.unwrap_or_default()).as_bytes(),
        )?;
        BeaconGenesis::generate(&config, l1_spec.genesis_header(), staging.path())?;
        let mut result = GenesisOutput {
            config,
            contracts: plan.contracts,
            settings: GenesisForge::digest(&serde_json::to_vec(&settings)?),
            files: BTreeMap::new(),
        };
        result.publish(staging.path(), &output)?;
        tracing::info!(directory = %output.display(), "generated Base genesis");
        Ok(())
    }

    /// Convert development activation blocks into the existing timestamp-based upgrade config.
    fn upgrades(config: &GenesisConfig, blocks: &[(BaseUpgrade, u64)]) -> Result<UpgradeConfig> {
        let denim = blocks
            .iter()
            .find_map(|(upgrade, block)| (*upgrade == BaseUpgrade::Denim).then_some(*block));
        let mut upgrades = UpgradeConfig::default();
        for &(upgrade, block) in blocks {
            let elapsed = if let Some(denim) = denim.filter(|denim| block > *denim) {
                ensure!(
                    (block - denim).is_multiple_of(5),
                    "{upgrade} must align to a whole second after Denim"
                );
                denim.checked_mul(2).and_then(|before| before.checked_add((block - denim) / 5))
            } else {
                block.checked_mul(2)
            };
            let timestamp = elapsed
                .and_then(|elapsed| {
                    config.timestamp.and_then(|genesis| genesis.checked_add(elapsed))
                })
                .ok_or_else(|| eyre::eyre!("{upgrade} activation timestamp overflow"))?;
            upgrades.set_activation_timestamp(upgrade, timestamp);
        }
        Ok(upgrades)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::Path,
        process::{Command, Output},
    };

    use base_common_genesis::RollupConfig;
    use base_execution_chainspec::BaseChainSpec;
    use tempfile::tempdir;

    use crate::GenesisOutput;

    fn workflow(directory: &Path, flags: &[&str]) -> Output {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..");
        Command::new("bash")
            .arg(root.join("etc/genesis/generate.sh"))
            .env("BASE_GENESIS_BIN", root.join("target/debug/base"))
            .args([
                "--artifacts-dir",
                root.join("build/genesis").to_str().unwrap(),
                "--output-dir",
                directory.to_str().unwrap(),
                "--timestamp",
                "1700000000",
            ])
            .args(flags)
            .output()
            .unwrap()
    }

    #[test]
    #[ignore = "requires prepared contracts and `cargo build -p base --features genesis`"]
    fn workflow_preserves_genesis_when_runtime_rollup_is_patched() {
        let directory = tempdir().unwrap();
        let first = workflow(directory.path(), &[]);
        assert!(first.status.success(), "{}", String::from_utf8_lossy(&first.stderr));
        let manifest_path = directory.path().join(".setup-complete");
        let manifest = fs::read(&manifest_path).unwrap();
        let output: GenesisOutput = serde_json::from_slice(&manifest).unwrap();
        let rollup_bytes = fs::read(directory.path().join("l2/rollup.json")).unwrap();
        let rollup: RollupConfig = serde_json::from_slice(&rollup_bytes).unwrap();
        let genesis =
            serde_json::from_slice(&fs::read(directory.path().join("l2/genesis.json")).unwrap())
                .unwrap();
        assert_eq!(
            rollup.genesis.l2.hash,
            BaseChainSpec::try_from_genesis(genesis).unwrap().genesis_hash()
        );
        let mut runtime: serde_json::Value = serde_json::from_slice(&rollup_bytes).unwrap();
        runtime["genesis"]["l1"]["hash"] =
            serde_json::json!(alloy_primitives::B256::with_last_byte(42));
        fs::write(
            directory.path().join("runtime-rollup.json"),
            serde_json::to_vec(&runtime).unwrap(),
        )
        .unwrap();
        let again = workflow(directory.path(), &[]);
        assert!(again.status.success(), "{}", String::from_utf8_lossy(&again.stderr));
        assert_eq!(manifest, fs::read(&manifest_path).unwrap());
        assert!(output.files.contains_key(&directory.path().join("l2/rollup.json")));
        fs::write(directory.path().join("l2/rollup.json"), b"corrupt").unwrap();
        assert!(!workflow(directory.path(), &[]).status.success());
    }

    #[test]
    #[ignore = "requires prepared contracts and `cargo build -p base --features genesis`"]
    fn workflow_supports_alternate_chains_and_upgrade_schedules() {
        for flags in [
            vec!["--isthmus-block", "20", "--zenith-block", "100"],
            vec!["--azul-block", "0", "--denim-block", "100", "--zenith-block", "105"],
        ] {
            let directory = tempdir().unwrap();
            let mut args =
                vec!["--l1-chain-id", "900", "--l2-chain-id", "901", "--upgrade-signal", "false"];
            args.extend(flags);
            let result = workflow(directory.path(), &args);
            assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
            let rollup: RollupConfig =
                serde_json::from_slice(&fs::read(directory.path().join("l2/rollup.json")).unwrap())
                    .unwrap();
            assert_eq!(rollup.l1_chain_id, 900);
            assert!(!directory.path().join("l2/upgrade-signal.env").exists());
        }
        let directory = tempdir().unwrap();
        assert!(
            !workflow(directory.path(), &["--denim-block", "25", "--zenith-block", "26"])
                .status
                .success()
        );
        assert!(!directory.path().join(".setup-complete").exists());
    }
}
