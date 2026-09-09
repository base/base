#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use alloy_primitives::{B256, keccak256};
use eyre::{Result, WrapErr, ensure};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::info;

use crate::{ContractArtifacts, GenesisArtifacts, GenesisConfig, GenesisGenerator};

/// Output paths and devnet-specific peer/runtime files.
#[derive(Debug, Clone, Serialize)]
pub struct GenesisOutput {
    /// L1 execution and beacon output root.
    pub l1: PathBuf,
    /// L2 configuration output root.
    pub l2: PathBuf,
    /// Shared genesis timestamp output root.
    pub shared: PathBuf,
    /// Fixed-name peer identity files.
    pub peers: BTreeMap<String, String>,
    /// Runtime settings for the deployed `ProtocolVersions` registry.
    pub upgrade_signal_env: String,
}

/// Completion manifest, published only after every output has been written.
#[derive(Debug, Serialize, Deserialize)]
pub struct GenesisCompletion {
    /// Generator format version.
    pub version: u32,
    /// Fingerprint of requested inputs, including explicit salt/time when supplied.
    pub fingerprint: B256,
    /// Checksums of the generated files.
    pub files: BTreeMap<PathBuf, B256>,
}

impl GenesisCompletion {
    /// Format version, including fork semantics and the generated file layout.
    pub const VERSION: u32 = 5;
}

impl GenesisOutput {
    /// Default system-test layout, with peer identities filled by the caller when needed.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let l1 = root.into();
        let defaults: BTreeMap<String, [String; 2]> =
            serde_json::from_str(include_str!("../assets/peer-files.json"))
                .expect("embedded peer defaults");
        Self {
            l2: l1.join("l2"),
            shared: l1.join("shared"),
            l1,
            peers: defaults
                .into_iter()
                .map(|(file, pair)| (file, format!("{}\n", pair[1])))
                .collect(),
            upgrade_signal_env: "BASE_NODE_UPGRADE_SIGNAL_L1_RPC=http://l1-el:4545\nBASE_NODE_UPGRADE_SIGNAL_MODE=runtime-admin\nBASE_NODE_UPGRADE_SIGNAL_L1_BLOCK_TAG=latest\n".into(),
        }
    }

    /// Generates a fresh configuration, or validates and reuses matching completed files.
    pub fn generate(&self, config: &GenesisConfig, contracts: &ContractArtifacts) -> Result<()> {
        config.validate()?;
        fs::create_dir_all(&self.l1)?;
        let lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.l1.join(".genesis.lock"))?;
        lock.try_lock().wrap_err("another generator is using this output directory")?;
        // Compose mounts L2 configs below its read-only L1 config mount.
        fs::create_dir_all(self.l1.join("l2"))?;
        let fingerprint = keccak256(serde_json::to_vec(&(config, self, contracts.fingerprint))?);
        let marker = self.l1.join(".setup-complete");
        if marker.exists() {
            let data = fs::read(&marker)?;
            let completion: GenesisCompletion = serde_json::from_slice(&data)
                .wrap_err("legacy setup; regenerate the disposable devnet configuration")?;
            ensure!(
                completion.version == GenesisCompletion::VERSION
                    && completion.fingerprint == fingerprint,
                "genesis format or inputs changed; regenerate the disposable devnet configuration"
            );
            ensure!(
                completion.files.len() >= 10 && fs::read(self.l2.join(".setup-complete"))
                    .wrap_err_with(|| format!(
                        "incomplete genesis configuration: missing L2 completion marker in {}; regenerate both L1 and L2 disposable configurations",
                        self.l2.display()
                    ))? == data,
                "incomplete genesis configuration; regenerate both L1 and L2 disposable configurations"
            );
            for (path, hash) in completion.files {
                ensure!(
                    keccak256(
                        fs::read(&path)
                            .wrap_err_with(|| format!("missing genesis file {}", path.display()))?
                    ) == hash,
                    "genesis file changed: {}",
                    path.display()
                );
            }
            info!("reusing completed devnet genesis");
            return Ok(());
        }
        // These read-only checks also work when output roots alias or contain one another.
        for root in [&self.l1, &self.l2, &self.shared] {
            ensure!(
                Self::empty_tree(root)?,
                "incomplete or legacy genesis directory {}; regenerate the disposable configuration",
                root.display()
            );
        }
        let artifacts = GenesisGenerator::generate(config, contracts)?;
        let mut files = artifacts.files(self)?;
        for (name, contents) in &self.peers {
            ensure!(
                Path::new(name).components().count() == 1 && !name.starts_with('.'),
                "invalid peer filename"
            );
            // The execution bootnode parses the entire file as a secret key,
            // unlike the node entrypoint's shell substitution which strips LF.
            let contents = if ["el-bootnode-p2p-key.txt", "cl-bootnode-p2p-key.txt"]
                .contains(&name.as_str())
            {
                contents.trim_end_matches(['\r', '\n'])
            } else {
                contents.as_str()
            };
            files.insert(self.l2.join(name), contents.as_bytes().to_vec());
        }
        if config.preinstall_upgrade_signal {
            files.insert(
                self.l2.join("upgrade-signal.env"),
                format!(
                    "BASE_NODE_UPGRADE_SIGNAL_CONTRACT={:#x}\n{}",
                    artifacts.rollup.protocol_versions_address, self.upgrade_signal_env
                )
                .into_bytes(),
            );
        }
        let completion = GenesisCompletion {
            version: GenesisCompletion::VERSION,
            fingerprint,
            files: files.iter().map(|(p, data)| (p.clone(), keccak256(data))).collect(),
        };
        for (path, data) in files {
            Self::write(&path, &data)?;
        }
        let completion = serde_json::to_vec(&completion)?;
        Self::write(&self.l2.join(".setup-complete"), &completion)?;
        Self::write(&marker, &completion)?;
        info!("devnet genesis complete");
        Ok(())
    }

    /// Checks for existing files without deleting caller-owned state.
    pub fn empty_tree(path: &Path) -> Result<bool> {
        if !path.exists() {
            return Ok(true);
        }
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            if entry.file_name() == ".genesis.lock" {
                continue;
            }
            if !entry.file_type()?.is_dir() || !Self::empty_tree(&entry.path())? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Atomically writes an individual file with restrictive permissions.
    pub fn write(path: &Path, bytes: &[u8]) -> Result<()> {
        let parent = path.parent().ok_or_else(|| eyre::eyre!("missing output parent"))?;
        fs::create_dir_all(parent)?;
        let mut file = tempfile::NamedTempFile::new_in(parent)?;
        file.write_all(bytes)?;
        #[cfg(unix)]
        {
            let secret = path
                .file_name()
                .is_some_and(|name| name.to_string_lossy().contains("key") || name == "jwt.hex")
                || path.components().any(|part| part.as_os_str() == "secrets");
            file.as_file().set_permissions(fs::Permissions::from_mode(if secret {
                0o600
            } else {
                0o644
            }))?;
        }
        file.as_file().sync_all()?;
        file.persist(path)?;
        Ok(())
    }
}

impl GenesisArtifacts {
    /// Renders the existing EL, CL, validator, and rollup file layout.
    pub fn files(&self, output: &GenesisOutput) -> Result<BTreeMap<PathBuf, Vec<u8>>> {
        let mut rollup = serde_json::to_value(&self.rollup)?;
        let system = self.rollup.genesis.system_config.as_ref().expect("generated system config");
        rollup["genesis"]["system_config"] = json!({
            "batcherAddr": system.batcher_address, "overhead": B256::from(system.overhead),
            "scalar": B256::from(system.scalar), "gasLimit": system.gas_limit,
            "operatorFeeParams": B256::ZERO, "minBaseFee": system.min_base_fee,
            "daFootprintGasScalar": system.da_footprint_gas_scalar,
        });
        rollup.as_object_mut().unwrap().remove("granite_channel_timeout");
        let mut conductor = rollup.clone();
        // Upstream conductor does not accept Base-only rollup fields.
        conductor.as_object_mut().unwrap().remove("base");
        let mut files = BTreeMap::new();
        for (path, value) in [
            (output.l1.join("el/genesis.json"), serde_json::to_value(&self.l1)?),
            (output.l1.join("el/chain-config.json"), serde_json::to_value(&self.l1.config)?),
            (output.l2.join("genesis.json"), serde_json::to_value(&self.l2)?),
            (output.l2.join("rollup.json"), rollup),
            (output.l2.join("rollup-conductor.json"), conductor),
            (output.l2.join("l1-addresses.json"), self.addresses.clone()),
        ] {
            files.insert(path, serde_json::to_vec(&value)?);
        }
        files.insert(output.l1.join("cl/genesis.ssz"), self.beacon.ssz.clone());
        files.insert(output.l1.join("cl/config.yaml"), self.beacon.config.as_bytes().to_vec());
        // Lighthouse requires this even with a preloaded genesis validator.
        files.insert(output.l1.join("cl/deposit_contract_block.txt"), b"0\n".to_vec());
        files.insert(
            output.l1.join("jwt.hex"),
            format!("{}\n", alloy_primitives::hex::encode(B256::random())).into_bytes(),
        );
        files.insert(
            output.shared.join("genesis_timestamp"),
            format!("{}\n", self.l1.timestamp).into_bytes(),
        );
        let pubkey = format!("0x{}", self.beacon.keystore.pubkey());
        files.insert(
            output.l1.join(format!("cl/validator_data/validators/{pubkey}/voting-keystore.json")),
            serde_json::to_vec(&self.beacon.keystore)?,
        );
        files.insert(
            output.l1.join(format!("cl/validator_data/secrets/{pubkey}")),
            self.beacon.password.as_bytes().to_vec(),
        );
        Ok(files)
    }
}
