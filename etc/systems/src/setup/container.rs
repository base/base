use std::{
    fs,
    io::ErrorKind,
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::{Duration, Instant},
};

use eyre::{Result, WrapErr, ensure};
use testcontainers::{
    GenericImage, ImageExt,
    core::{Mount, WaitFor, wait::ExitWaitStrategy},
    runners::SyncRunner,
};

use crate::config::{BATCHER, BUILDER, CHALLENGER, DEPLOYER, PROPOSER, SEQUENCER};

const SETUP_IMAGE_NAME: &str = "devnet-setup";
// Bump this interface version when cached setup images can no longer serve callers.
// v2 invokes the integrated offline generator as `op-deployer` without a subcommand.
const SETUP_IMAGE_TAG: &str = "local-v2";
const SETUP_IMAGE_REFERENCE: &str = "devnet-setup:local-v2";
const SETUP_IMAGE_BUILD_LOCK_DIR: &str = "base-system-test-setup-image-build.lock";
const SETUP_IMAGE_BUILD_LOCK_TIMEOUT: Duration = Duration::from_secs(600);
const SETUP_IMAGE_BUILD_LOCK_POLL_INTERVAL: Duration = Duration::from_millis(500);
const SETUP_DOCKERFILE_PATH: &str = "etc/docker/Dockerfile.devnet";
const SETUP_TIMEOUT_SECS: u64 = 300;

/// Builder enode ID
pub const BUILDER_ENODE_ID: &str = "3255458e24278e31d5940f304b16300fdff3f6efd3e2a030b5818310ac67af45e28d057e6a332d07e0c5ab09d6947fd4eed1a646edbf224e2d2fec6f49f90abc";
/// Execution-layer bootnode private key.
pub const EL_BOOTNODE_P2P_KEY: &str =
    "1111111111111111111111111111111111111111111111111111111111111111";
/// Execution-layer bootnode enode ID.
pub const EL_BOOTNODE_ENODE_ID: &str = "4f355bdcb7cc0af728ef3cceb9615d90684bb5b2ca5f859ab0f0b704075871aa385b6b1b8ead809ca67454d9683fcf2ba03456d6fe2c4abe2b07f0fbdbb2f1c1";
/// Execution-layer bootnode enode URL used by setup templates.
pub const EL_BOOTNODE_ENODE: &str = "enode://4f355bdcb7cc0af728ef3cceb9615d90684bb5b2ca5f859ab0f0b704075871aa385b6b1b8ead809ca67454d9683fcf2ba03456d6fe2c4abe2b07f0fbdbb2f1c1@172.30.0.10:9303";
/// Consensus-layer bootnode private key.
pub const CL_BOOTNODE_P2P_KEY: &str =
    "2222222222222222222222222222222222222222222222222222222222222222";
/// Consensus-layer bootnode ENR output path in the shared bootnode volume.
pub const CL_BOOTNODE_ENR_PATH: &str = "/bootnodes/cl-bootnode.enr";

/// Docker image used to generate system test genesis and deployment artifacts.
#[derive(Debug, Clone, Copy)]
pub struct SetupImage;

impl SetupImage {
    /// Returns a testcontainers image request for the setup image.
    pub fn request() -> GenericImage {
        GenericImage::new(SETUP_IMAGE_NAME, SETUP_IMAGE_TAG)
    }

    /// Returns whether the setup image is available locally.
    pub fn exists() -> bool {
        Command::new("docker")
            .args(["image", "inspect", SETUP_IMAGE_REFERENCE])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// Builds the setup image if it is not available locally.
    pub fn ensure_built() -> Result<()> {
        if Self::exists() {
            return Ok(());
        }

        let lock_dir = std::env::temp_dir().join(SETUP_IMAGE_BUILD_LOCK_DIR);
        let lock_started = Instant::now();
        loop {
            match fs::create_dir(&lock_dir) {
                Ok(()) => break,
                Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                    if Self::exists() {
                        return Ok(());
                    }
                    ensure!(
                        lock_started.elapsed() < SETUP_IMAGE_BUILD_LOCK_TIMEOUT,
                        "timed out waiting for setup image build lock at {}",
                        lock_dir.display(),
                    );
                    thread::sleep(SETUP_IMAGE_BUILD_LOCK_POLL_INTERVAL);
                }
                Err(error) => {
                    return Err(error).wrap_err("Failed to acquire setup image build lock");
                }
            }
        }

        let build_result = (|| {
            if Self::exists() {
                return Ok(());
            }

            let repo_root = Self::find_repo_root()?;
            let dockerfile_path = repo_root.join(SETUP_DOCKERFILE_PATH);

            ensure!(dockerfile_path.exists(), "{SETUP_DOCKERFILE_PATH} not found");

            let status = Command::new("docker")
                .args(["build", "-t", SETUP_IMAGE_REFERENCE, "-f", SETUP_DOCKERFILE_PATH, "."])
                .current_dir(&repo_root)
                .status()
                .wrap_err("Failed to run docker build")?;

            ensure!(status.success(), "docker build failed");

            Ok(())
        })();

        let cleanup_result =
            fs::remove_dir(&lock_dir).wrap_err("Failed to release setup image build lock");

        match build_result {
            Ok(()) => cleanup_result,
            Err(error) => {
                let _ = cleanup_result;
                Err(error)
            }
        }
    }

    /// Finds the repository root that contains the setup Dockerfile.
    ///
    /// Resolved from this crate's `CARGO_MANIFEST_DIR` (fixed at compile time) rather than the
    /// process's current directory, so this still finds the Dockerfile when `base-system-tests`
    /// is compiled as a vendored git dependency from another workspace — where the process's
    /// current directory is the consuming repo, not this one.
    pub fn find_repo_root() -> Result<PathBuf> {
        let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .ok_or_else(|| eyre::eyre!("CARGO_MANIFEST_DIR has no grandparent directory"))?
            .to_path_buf();
        ensure!(
            repo_root.join(SETUP_DOCKERFILE_PATH).exists(),
            "{SETUP_DOCKERFILE_PATH} not found under {}",
            repo_root.display()
        );
        Ok(repo_root)
    }
}

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

/// A container for generating both chains with the offline Base op-deployer.
#[derive(Debug, Clone)]
pub struct SetupContainer {
    output_dir: PathBuf,
    chain_id: u64,
    l2_chain_id: u64,
    slot_duration: u64,
    isthmus_activation_block: Option<u64>,
    base_azul_activation_block: Option<u64>,
    base_beryl_activation_block: Option<u64>,
    base_cobalt_activation_block: Option<u64>,
    base_denim_activation_block: Option<u64>,
    base_zenith_activation_block: Option<u64>,
}

impl SetupContainer {
    /// Creates a new setup container with the given output directory.
    pub fn new(output_dir: impl Into<PathBuf>) -> Self {
        Self {
            output_dir: output_dir.into(),
            chain_id: 1337,
            l2_chain_id: 84538453,
            slot_duration: 1,
            isthmus_activation_block: None,
            base_azul_activation_block: None,
            base_beryl_activation_block: None,
            base_cobalt_activation_block: None,
            base_denim_activation_block: None,
            base_zenith_activation_block: None,
        }
    }

    /// Sets the L1 chain ID.
    pub const fn with_chain_id(mut self, chain_id: u64) -> Self {
        self.chain_id = chain_id;
        self
    }

    /// Sets the L2 chain ID.
    pub const fn with_l2_chain_id(mut self, l2_chain_id: u64) -> Self {
        self.l2_chain_id = l2_chain_id;
        self
    }

    /// Sets the slot duration.
    pub const fn with_slot_duration(mut self, slot_duration: u64) -> Self {
        self.slot_duration = slot_duration;
        self
    }

    /// Sets the L2 block number at which Isthmus activates.
    pub const fn with_isthmus_activation_block(mut self, block: u64) -> Self {
        self.isthmus_activation_block = Some(block);
        self
    }

    /// Sets the L2 block number at which Base Azul activates.
    pub const fn with_base_azul_activation_block(mut self, block: u64) -> Self {
        self.base_azul_activation_block = Some(block);
        self
    }

    /// Sets the L2 block number at which Base Beryl activates.
    pub const fn with_base_beryl_activation_block(mut self, block: u64) -> Self {
        self.base_beryl_activation_block = Some(block);
        self
    }

    /// Sets the L2 block number at which Base Cobalt activates.
    pub const fn with_base_cobalt_activation_block(mut self, block: u64) -> Self {
        self.base_cobalt_activation_block = Some(block);
        self
    }

    /// Sets the L2 block number at which Base Denim activates.
    pub const fn with_base_denim_activation_block(mut self, block: u64) -> Self {
        self.base_denim_activation_block = Some(block);
        self
    }

    /// Sets the L2 block number at which the genesis-only Base Zenith testing gate activates.
    pub const fn with_base_zenith_activation_block(mut self, block: u64) -> Self {
        self.base_zenith_activation_block = Some(block);
        self
    }

    /// Generates both chains before starting L1, with deployed contracts in genesis.
    pub fn generate_genesis(&self) -> Result<(L1GenesisOutput, L2DeploymentOutput)> {
        SetupImage::ensure_built()?;
        fs::create_dir_all(&self.output_dir).wrap_err("Failed to create output directory")?;
        let output_dir =
            self.output_dir.canonicalize().wrap_err("Failed to canonicalize output directory")?;
        let output_mount = output_dir.to_string_lossy().to_string();

        let mut container = SetupImage::request()
            .with_wait_for(WaitFor::exit(ExitWaitStrategy::default().with_exit_code(0)))
            .with_startup_timeout(Duration::from_secs(SETUP_TIMEOUT_SECS))
            .with_network("none")
            .with_env_var("OUTPUT_DIR", "/output")
            .with_env_var("L2_OUTPUT_DIR", "/output/l2")
            .with_env_var("SHARED_DIR", "/output/shared")
            .with_env_var("CHAIN_ID", self.chain_id.to_string())
            .with_env_var("L2_CHAIN_ID", self.l2_chain_id.to_string())
            .with_env_var("SLOT_DURATION", self.slot_duration.to_string())
            // Upgrade-signal tests deploy their own configurable mock after L1 starts.
            .with_env_var("UPGRADE_SIGNAL_PREINSTALL", "false")
            .with_env_var("DEPLOYER_ADDR", format!("{:#x}", DEPLOYER.address))
            .with_env_var("SEQUENCER_ADDR", format!("{:#x}", SEQUENCER.address))
            .with_env_var("BATCHER_ADDR", format!("{:#x}", BATCHER.address))
            .with_env_var("PROPOSER_ADDR", format!("{:#x}", PROPOSER.address))
            .with_env_var("CHALLENGER_ADDR", format!("{:#x}", CHALLENGER.address))
            .with_env_var("BUILDER_P2P_KEY", format!("{:#x}", BUILDER.private_key))
            .with_env_var("BUILDER_ENODE_ID", BUILDER_ENODE_ID)
            .with_env_var("L2_EL_BOOTNODE_P2P_KEY", EL_BOOTNODE_P2P_KEY)
            .with_env_var("L2_EL_BOOTNODE_ENODE_ID", EL_BOOTNODE_ENODE_ID)
            .with_env_var("L2_EL_BOOTNODE_ENODE", EL_BOOTNODE_ENODE)
            .with_env_var("L2_CL_BOOTNODE_P2P_KEY", CL_BOOTNODE_P2P_KEY)
            .with_env_var("L2_CL_BOOTNODE_ENR_PATH", CL_BOOTNODE_ENR_PATH);

        if let Some(block) = self.isthmus_activation_block {
            container = container.with_env_var("L2_ISTHMUS_BLOCK", block.to_string());
        }

        if let Some(block) = self.base_azul_activation_block {
            container = container.with_env_var("L2_BASE_AZUL_BLOCK", block.to_string());
        }

        if let Some(block) = self.base_beryl_activation_block {
            container = container.with_env_var("L2_BASE_BERYL_BLOCK", block.to_string());
        }

        if let Some(block) = self.base_cobalt_activation_block {
            container = container.with_env_var("L2_BASE_COBALT_BLOCK", block.to_string());
        }

        if let Some(block) = self.base_denim_activation_block {
            container = container.with_env_var("L2_BASE_DENIM_BLOCK", block.to_string());
        }

        if let Some(block) = self.base_zenith_activation_block {
            container = container.with_env_var("L2_BASE_ZENITH_BLOCK", block.to_string());
        }

        let _container = container
            .with_mount(Mount::bind_mount(output_mount, "/output"))
            .with_cmd(["op-deployer"])
            .start()
            .wrap_err("Failed to generate devnet genesis with op-deployer")?;

        ensure!(self.output_dir.join("cl/genesis.ssz").exists(), "genesis.ssz was not generated");
        ensure!(
            self.output_dir.join("l2/genesis.json").exists(),
            "L2 genesis.json was not generated"
        );
        Ok((
            L1GenesisOutput { output_dir: self.output_dir.clone() },
            L2DeploymentOutput { output_dir: self.output_dir.clone() },
        ))
    }
}
