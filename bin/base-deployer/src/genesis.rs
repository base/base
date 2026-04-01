//! Genesis artifact generation for `base-deployer`.

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use eyre::{ContextCompat, Result, WrapErr, ensure};
use serde::Serialize;
use serde_json::{Value, json};

use crate::config::{ChainIds, ResolvedConfig};
use crate::devnet::{
    TEST_MNEMONIC, derived_accounts, l1_beacon_config_yaml, l1_el_genesis, l2_intent_toml,
};

/// Paths to genesis artifacts produced by `base-deployer genesis`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GenesisArtifacts {
    /// Root output directory.
    pub(crate) output_dir: PathBuf,
    /// L1 EL genesis file.
    pub(crate) l1_el_genesis: PathBuf,
    /// L1 chain config file.
    pub(crate) l1_chain_config: PathBuf,
    /// L1 CL config file.
    pub(crate) l1_cl_config: PathBuf,
    /// L1 CL genesis state.
    pub(crate) l1_cl_genesis: PathBuf,
    /// L2 intent file.
    pub(crate) l2_intent: PathBuf,
    /// L2 genesis file.
    pub(crate) l2_genesis: PathBuf,
    /// L2 rollup config.
    pub(crate) l2_rollup: PathBuf,
    /// L2 rollup config for op-conductor.
    pub(crate) l2_rollup_conductor: PathBuf,
    /// L1 contract address manifest.
    pub(crate) l1_addresses: PathBuf,
    /// JWT secret file.
    pub(crate) jwt_secret: PathBuf,
    /// Chain IDs metadata.
    pub(crate) chain_ids: PathBuf,
    /// Funded account manifest.
    pub(crate) accounts_manifest: PathBuf,
}

/// Generates devnet genesis artifacts using the default command-based tooling backend.
pub(crate) fn generate_genesis(config: &ResolvedConfig) -> Result<GenesisArtifacts> {
    generate_genesis_with_tooling(config, &CommandTooling)
}

fn generate_genesis_with_tooling(
    config: &ResolvedConfig,
    tooling: &impl GenesisTooling,
) -> Result<GenesisArtifacts> {
    let paths = GenesisPaths::new(&config.output_dir);
    paths.create_directories()?;

    write_chain_ids(&paths.chain_ids, config.chain_ids())?;
    write_accounts_manifest(&paths.accounts_manifest)?;
    write_jwt_secret(&paths.jwt_secret)?;
    write_l1_execution_genesis(config, &paths.l1_el_genesis, &paths.l1_chain_config)?;
    write_l1_consensus_inputs(config, &paths.l1_cl_config, &paths.l1_mnemonics)?;
    write_l2_intent(config, &paths.l2_intent)?;

    tooling.generate_l1_consensus_artifacts(&paths)?;
    tooling.generate_l2_genesis_artifacts(config, &paths)?;
    patch_base_v1_activation(config, &paths)?;
    write_rollup_conductor(&paths)?;

    Ok(paths.into_artifacts())
}

#[derive(Debug, Clone)]
struct GenesisPaths {
    output_dir: PathBuf,
    l1_cl_dir: PathBuf,
    l1_el_dir: PathBuf,
    l2_dir: PathBuf,
    l1_el_genesis: PathBuf,
    l1_chain_config: PathBuf,
    l1_cl_config: PathBuf,
    l1_cl_genesis: PathBuf,
    l1_mnemonics: PathBuf,
    l2_intent: PathBuf,
    l2_genesis: PathBuf,
    l2_rollup: PathBuf,
    l2_rollup_conductor: PathBuf,
    l1_addresses: PathBuf,
    jwt_secret: PathBuf,
    chain_ids: PathBuf,
    accounts_manifest: PathBuf,
}

impl GenesisPaths {
    fn new(output_dir: &Path) -> Self {
        let l1_dir = output_dir.join("l1");
        let l1_el_dir = l1_dir.join("el");
        let l1_cl_dir = l1_dir.join("cl");
        let l2_dir = output_dir.join("l2");

        Self {
            output_dir: output_dir.to_path_buf(),
            l1_cl_dir: l1_cl_dir.clone(),
            l1_el_dir: l1_el_dir.clone(),
            l2_dir: l2_dir.clone(),
            l1_el_genesis: l1_el_dir.join("genesis.json"),
            l1_chain_config: l1_el_dir.join("chain-config.json"),
            l1_cl_config: l1_cl_dir.join("config.yaml"),
            l1_cl_genesis: l1_cl_dir.join("genesis.ssz"),
            l1_mnemonics: l1_cl_dir.join("mnemonics.yaml"),
            l2_intent: l2_dir.join("intent.toml"),
            l2_genesis: l2_dir.join("genesis.json"),
            l2_rollup: l2_dir.join("rollup.json"),
            l2_rollup_conductor: l2_dir.join("rollup-conductor.json"),
            l1_addresses: l2_dir.join("l1-addresses.json"),
            jwt_secret: l1_dir.join("jwt.hex"),
            chain_ids: output_dir.join("chain-ids.json"),
            accounts_manifest: output_dir.join("accounts.json"),
        }
    }

    fn create_directories(&self) -> Result<()> {
        fs::create_dir_all(&self.l1_el_dir)
            .wrap_err_with(|| format!("Failed to create {}", self.l1_el_dir.display()))?;
        fs::create_dir_all(&self.l1_cl_dir)
            .wrap_err_with(|| format!("Failed to create {}", self.l1_cl_dir.display()))?;
        fs::create_dir_all(&self.l2_dir)
            .wrap_err_with(|| format!("Failed to create {}", self.l2_dir.display()))?;
        Ok(())
    }

    fn into_artifacts(self) -> GenesisArtifacts {
        GenesisArtifacts {
            output_dir: self.output_dir,
            l1_el_genesis: self.l1_el_genesis,
            l1_chain_config: self.l1_chain_config,
            l1_cl_config: self.l1_cl_config,
            l1_cl_genesis: self.l1_cl_genesis,
            l2_intent: self.l2_intent,
            l2_genesis: self.l2_genesis,
            l2_rollup: self.l2_rollup,
            l2_rollup_conductor: self.l2_rollup_conductor,
            l1_addresses: self.l1_addresses,
            jwt_secret: self.jwt_secret,
            chain_ids: self.chain_ids,
            accounts_manifest: self.accounts_manifest,
        }
    }
}

trait GenesisTooling {
    fn generate_l1_consensus_artifacts(&self, paths: &GenesisPaths) -> Result<()>;

    fn generate_l2_genesis_artifacts(
        &self,
        config: &ResolvedConfig,
        paths: &GenesisPaths,
    ) -> Result<()>;
}

struct CommandTooling;

impl GenesisTooling for CommandTooling {
    fn generate_l1_consensus_artifacts(&self, paths: &GenesisPaths) -> Result<()> {
        run_command(
            Command::new("eth-genesis-state-generator")
                .arg("beaconchain")
                .arg("--eth1-config")
                .arg(&paths.l1_el_genesis)
                .arg("--config")
                .arg(&paths.l1_cl_config)
                .arg("--mnemonics")
                .arg(&paths.l1_mnemonics)
                .arg("--state-output")
                .arg(&paths.l1_cl_genesis),
            "generate L1 beacon genesis state",
        )?;

        let validator_keys_dir = paths.l1_cl_dir.join("validator_keys");
        let validator_data_dir = paths.l1_cl_dir.join("validator_data");

        if validator_keys_dir.exists() {
            fs::remove_dir_all(&validator_keys_dir)
                .wrap_err_with(|| format!("Failed to remove {}", validator_keys_dir.display()))?;
        }
        if validator_data_dir.exists() {
            fs::remove_dir_all(&validator_data_dir)
                .wrap_err_with(|| format!("Failed to remove {}", validator_data_dir.display()))?;
        }

        run_command(
            Command::new("eth2-val-tools")
                .arg("keystores")
                .arg("--insecure")
                .arg(format!("--source-mnemonic={TEST_MNEMONIC}"))
                .arg("--source-min=0")
                .arg("--source-max=1")
                .arg(format!("--out-loc={}", validator_keys_dir.display())),
            "generate validator keystores",
        )?;

        reorganize_validator_data(&validator_keys_dir, &validator_data_dir)?;
        fs::write(paths.l1_cl_dir.join("deploy_block.txt"), "0\n")
            .wrap_err("Failed to write deploy_block.txt")?;
        fs::write(paths.l1_cl_dir.join("deposit_contract_block.txt"), "0\n")
            .wrap_err("Failed to write deposit_contract_block.txt")?;

        Ok(())
    }

    fn generate_l2_genesis_artifacts(
        &self,
        config: &ResolvedConfig,
        paths: &GenesisPaths,
    ) -> Result<()> {
        let workdir = paths.output_dir.join(".op-deployer-genesis");
        if workdir.exists() {
            fs::remove_dir_all(&workdir)
                .wrap_err_with(|| format!("Failed to remove {}", workdir.display()))?;
        }
        fs::create_dir_all(&workdir)
            .wrap_err_with(|| format!("Failed to create {}", workdir.display()))?;

        run_command(
            Command::new("op-deployer")
                .arg("init")
                .arg("--l1-chain-id")
                .arg(config.l1_chain_id.to_string())
                .arg("--l2-chain-ids")
                .arg(config.l2_chain_id.to_string())
                .arg("--intent-type")
                .arg("custom")
                .arg("--workdir")
                .arg(&workdir),
            "initialize op-deployer workdir",
        )?;

        fs::copy(&paths.l2_intent, workdir.join("intent.toml"))
            .wrap_err("Failed to copy intent.toml into op-deployer workdir")?;

        run_command(
            Command::new("op-deployer")
                .arg("apply")
                .arg("--workdir")
                .arg(&workdir)
                .arg("--deployment-target")
                .arg("genesis"),
            "generate offline L2 deployment artifacts",
        )?;

        capture_stdout_to_path(
            Command::new("op-deployer")
                .arg("inspect")
                .arg("genesis")
                .arg("--workdir")
                .arg(&workdir)
                .arg(config.l2_chain_id.to_string()),
            &paths.l2_genesis,
            "inspect L2 genesis",
        )?;
        capture_stdout_to_path(
            Command::new("op-deployer")
                .arg("inspect")
                .arg("rollup")
                .arg("--workdir")
                .arg(&workdir)
                .arg(config.l2_chain_id.to_string()),
            &paths.l2_rollup,
            "inspect rollup config",
        )?;
        capture_stdout_to_path(
            Command::new("op-deployer")
                .arg("inspect")
                .arg("l1")
                .arg("--workdir")
                .arg(&workdir)
                .arg(config.l2_chain_id.to_string()),
            &paths.l1_addresses,
            "inspect L1 contract addresses",
        )?;

        Ok(())
    }
}

fn write_chain_ids(path: &Path, chain_ids: ChainIds) -> Result<()> {
    write_json(path, &chain_ids)
}

fn write_accounts_manifest(path: &Path) -> Result<()> {
    #[derive(Serialize)]
    struct AccountEntry {
        name: &'static str,
        address: String,
        private_key: String,
    }

    #[derive(Serialize)]
    struct AccountsManifest {
        mnemonic: &'static str,
        funded_accounts: Vec<AccountEntry>,
    }

    let accounts = derived_accounts();
    let funded_accounts = [
        ("deployer", accounts[0]),
        ("account-1", accounts[1]),
        ("account-2", accounts[2]),
        ("account-3", accounts[3]),
        ("account-4", accounts[4]),
        ("sequencer", accounts[5]),
        ("batcher", accounts[6]),
        ("proposer", accounts[7]),
        ("challenger", accounts[8]),
        ("builder", accounts[9]),
    ]
    .into_iter()
    .map(|(name, account)| AccountEntry {
        name,
        address: format!("{:#x}", account.address),
        private_key: format!("0x{}", hex::encode(account.private_key)),
    })
    .collect();

    let manifest = AccountsManifest { mnemonic: TEST_MNEMONIC, funded_accounts };
    write_json(path, &manifest)
}

fn write_jwt_secret(path: &Path) -> Result<()> {
    let secret: [u8; 32] = rand::random();
    fs::write(path, format!("{}\n", hex::encode(secret)))
        .wrap_err_with(|| format!("Failed to write {}", path.display()))
}

fn write_l1_execution_genesis(
    config: &ResolvedConfig,
    genesis_path: &Path,
    chain_config_path: &Path,
) -> Result<()> {
    let genesis = l1_el_genesis(config.l1_chain_id, config.genesis_time, config.prefund_balance);
    let chain_config = genesis
        .get("config")
        .cloned()
        .context("Generated L1 genesis is missing the `config` field")?;

    write_json(genesis_path, &genesis)?;
    write_json(chain_config_path, &chain_config)?;
    Ok(())
}

fn write_l1_consensus_inputs(
    config: &ResolvedConfig,
    beacon_config_path: &Path,
    mnemonics_path: &Path,
) -> Result<()> {
    let beacon_config =
        l1_beacon_config_yaml(config.l1_chain_id, config.genesis_time, config.slot_duration);
    fs::write(beacon_config_path, beacon_config)
        .wrap_err_with(|| format!("Failed to write {}", beacon_config_path.display()))?;
    fs::write(
        mnemonics_path,
        format!("- mnemonic: \"{TEST_MNEMONIC}\"\n  count: 1\n"),
    )
    .wrap_err_with(|| format!("Failed to write {}", mnemonics_path.display()))
}

fn write_l2_intent(config: &ResolvedConfig, path: &Path) -> Result<()> {
    let intent = l2_intent_toml(config.l1_chain_id, config.l2_chain_id);
    fs::write(path, intent).wrap_err_with(|| format!("Failed to write {}", path.display()))
}

fn patch_base_v1_activation(config: &ResolvedConfig, paths: &GenesisPaths) -> Result<()> {
    let Some(base_v1_block) = config.l2_base_v1_block else {
        return Ok(());
    };

    let mut rollup: Value = read_json(&paths.l2_rollup)?;
    let mut genesis: Value = read_json(&paths.l2_genesis)?;

    let block_time = rollup
        .get("block_time")
        .and_then(Value::as_u64)
        .context("rollup.json is missing `block_time`")?;
    let l2_genesis_time = rollup
        .get("genesis")
        .and_then(Value::as_object)
        .and_then(|genesis| genesis.get("l2_time"))
        .and_then(Value::as_u64)
        .context("rollup.json is missing `genesis.l2_time`")?;

    let base_v1_time = l2_genesis_time + (block_time * base_v1_block);
    let rollup_base = rollup
        .as_object_mut()
        .expect("rollup config should be a JSON object")
        .entry("base")
        .or_insert_with(|| json!({}));
    rollup_base
        .as_object_mut()
        .expect("rollup base config should be an object")
        .insert("v1".to_string(), Value::from(base_v1_time));

    let genesis_config = genesis
        .get_mut("config")
        .and_then(Value::as_object_mut)
        .context("genesis.json is missing `config`")?;
    genesis_config.insert("osakaTime".to_string(), Value::from(base_v1_time));
    let base_config = genesis_config.entry("base".to_string()).or_insert_with(|| json!({}));
    base_config
        .as_object_mut()
        .expect("genesis base config should be an object")
        .insert("v1".to_string(), Value::from(base_v1_time));

    write_json(&paths.l2_rollup, &rollup)?;
    write_json(&paths.l2_genesis, &genesis)?;
    Ok(())
}

fn write_rollup_conductor(paths: &GenesisPaths) -> Result<()> {
    let mut rollup: Value = read_json(&paths.l2_rollup)?;
    if let Some(object) = rollup.as_object_mut() {
        object.remove("base");
    }
    write_json(&paths.l2_rollup_conductor, &rollup)
}

fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let contents = serde_json::to_string_pretty(value).wrap_err("Failed to serialize JSON")?;
    fs::write(path, format!("{contents}\n"))
        .wrap_err_with(|| format!("Failed to write {}", path.display()))
}

fn read_json(path: &Path) -> Result<Value> {
    let contents = fs::read_to_string(path)
        .wrap_err_with(|| format!("Failed to read {}", path.display()))?;
    serde_json::from_str(&contents)
        .wrap_err_with(|| format!("Failed to parse JSON at {}", path.display()))
}

fn reorganize_validator_data(validator_keys_dir: &Path, validator_data_dir: &Path) -> Result<()> {
    let validators_dir = validator_data_dir.join("validators");
    let secrets_dir = validator_data_dir.join("secrets");
    fs::create_dir_all(&validators_dir)
        .wrap_err_with(|| format!("Failed to create {}", validators_dir.display()))?;
    fs::create_dir_all(&secrets_dir)
        .wrap_err_with(|| format!("Failed to create {}", secrets_dir.display()))?;

    let keys_root = validator_keys_dir.join("keys");
    let secrets_root = validator_keys_dir.join("secrets");

    for entry in fs::read_dir(&keys_root)
        .wrap_err_with(|| format!("Failed to read {}", keys_root.display()))?
    {
        let entry = entry.wrap_err("Failed to read validator key entry")?;
        let key_dir = entry.path();
        if !key_dir.is_dir() {
            continue;
        }

        let pubkey = entry.file_name();
        let pubkey_str = pubkey.to_string_lossy();
        let validator_dir = validators_dir.join(pubkey_str.as_ref());
        fs::create_dir_all(&validator_dir)
            .wrap_err_with(|| format!("Failed to create {}", validator_dir.display()))?;

        fs::copy(key_dir.join("voting-keystore.json"), validator_dir.join("voting-keystore.json"))
            .wrap_err("Failed to copy voting-keystore.json")?;

        let secret_src = secrets_root.join(pubkey_str.as_ref());
        if secret_src.exists() {
            fs::copy(&secret_src, secrets_dir.join(pubkey_str.as_ref()))
                .wrap_err("Failed to copy validator password")?;
        }
    }

    Ok(())
}

fn run_command(command: &mut Command, purpose: &str) -> Result<()> {
    command.stdout(Stdio::null()).stderr(Stdio::piped());
    let output = command.output().wrap_err_with(|| format!("Failed to {purpose}"))?;
    ensure!(
        output.status.success(),
        "{purpose} failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(())
}

fn capture_stdout_to_path(command: &mut Command, path: &Path, purpose: &str) -> Result<()> {
    let output = command.output().wrap_err_with(|| format!("Failed to {purpose}"))?;
    ensure!(
        output.status.success(),
        "{purpose} failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    fs::write(path, output.stdout).wrap_err_with(|| format!("Failed to write {}", path.display()))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use serde_json::Value;
    use tempfile::TempDir;

    use super::{GenesisPaths, generate_genesis_with_tooling, read_json};
    use crate::config::{DeployerConfig, ResolvedConfig};

    struct FakeTooling;

    impl super::GenesisTooling for FakeTooling {
        fn generate_l1_consensus_artifacts(&self, paths: &GenesisPaths) -> eyre::Result<()> {
            fs::write(&paths.l1_cl_genesis, "fake-ssz")?;
            let validator_dir = paths.l1_cl_dir.join("validator_data/validators/fakepubkey");
            let secrets_dir = paths.l1_cl_dir.join("validator_data/secrets");
            fs::create_dir_all(&validator_dir)?;
            fs::create_dir_all(&secrets_dir)?;
            fs::write(validator_dir.join("voting-keystore.json"), "{}")?;
            fs::write(secrets_dir.join("fakepubkey"), "password")?;
            fs::write(paths.l1_cl_dir.join("deploy_block.txt"), "0\n")?;
            fs::write(paths.l1_cl_dir.join("deposit_contract_block.txt"), "0\n")?;
            Ok(())
        }

        fn generate_l2_genesis_artifacts(
            &self,
            config: &ResolvedConfig,
            paths: &GenesisPaths,
        ) -> eyre::Result<()> {
            fs::write(
                &paths.l2_genesis,
                serde_json::to_string_pretty(&serde_json::json!({
                    "config": {
                        "chainId": config.l2_chain_id,
                        "osakaTime": 0,
                    }
                }))?,
            )?;
            fs::write(
                &paths.l2_rollup,
                serde_json::to_string_pretty(&serde_json::json!({
                    "block_time": 2,
                    "genesis": {
                        "l2_time": config.genesis_time,
                    }
                }))?,
            )?;
            fs::write(
                &paths.l1_addresses,
                serde_json::to_string_pretty(&serde_json::json!({
                    "OptimismPortalProxy": format!("{:#x}", crate::devnet::role_accounts().deployer.address),
                    "SystemConfigProxy": format!("{:#x}", crate::devnet::role_accounts().sequencer.address),
                }))?,
            )?;
            Ok(())
        }
    }

    #[test]
    fn generates_expected_artifacts() {
        let tempdir = TempDir::new().expect("tempdir should be created");
        let config = DeployerConfig {
            output_dir: Some(tempdir.path().join("artifacts")),
            l1_chain_id: Some(1337),
            l2_chain_id: Some(84538453),
            slot_duration: Some(4),
            genesis_time: Some(1_715_000_000),
            prefund_balance: Some("0x10".to_string()),
            l2_base_v1_block: Some(20),
        }
        .resolve(None)
        .expect("config should resolve");

        let artifacts =
            generate_genesis_with_tooling(&config, &FakeTooling).expect("genesis should succeed");

        assert!(artifacts.l1_el_genesis.exists());
        assert!(artifacts.l1_cl_config.exists());
        assert!(artifacts.l1_cl_genesis.exists());
        assert!(artifacts.l2_intent.exists());
        assert!(artifacts.l2_genesis.exists());
        assert!(artifacts.l2_rollup.exists());
        assert!(artifacts.l2_rollup_conductor.exists());
        assert!(artifacts.l1_addresses.exists());
        assert!(artifacts.jwt_secret.exists());

        let l1_genesis = read_json(&artifacts.l1_el_genesis).expect("l1 genesis should parse");
        let alloc = l1_genesis
            .get("alloc")
            .and_then(Value::as_object)
            .expect("alloc should exist");
        assert!(alloc.contains_key("0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266"));

        let chain_ids = read_json(&artifacts.chain_ids).expect("chain ids should parse");
        assert_eq!(chain_ids["l1_chain_id"], 1337);
        assert_eq!(chain_ids["l2_chain_id"], 84538453);

        let rollup = read_json(&artifacts.l2_rollup).expect("rollup should parse");
        assert_eq!(rollup["base"]["v1"], 1_715_000_040u64);

        let rollup_conductor =
            read_json(&artifacts.l2_rollup_conductor).expect("rollup conductor should parse");
        assert!(rollup_conductor.get("base").is_none());
    }
}
