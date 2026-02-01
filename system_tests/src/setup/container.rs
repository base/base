use std::{path::PathBuf, process::Command, time::Duration};

use eyre::{Result, WrapErr, ensure};
use testcontainers::{
    GenericImage, ImageExt,
    core::{Mount, WaitFor, wait::ExitWaitStrategy},
    runners::SyncRunner,
};

use crate::{
    config::{BATCHER, CHALLENGER, DEPLOYER, PROPOSER, SEQUENCER},
    network::{ensure_network_exists, network_name},
};

const SETUP_IMAGE_TAG: &str = "devnet-setup:local";
const DEPLOY_TIMEOUT_SECS: u64 = 300;

/// Builder P2P private key (derived from Anvil Account 9, no 0x prefix for reth).
pub const BUILDER_P2P_KEY: &str =
    "2a871d0798f97d79848a013d4936a73bf4cc922c825d33c1cf7073dff6d409c6";

/// Builder enode ID (public key derived from BUILDER_P2P_KEY).
pub const BUILDER_ENODE_ID: &str = "8318535b54105d4a7aae60c08fc45f9687181b4fdfc625bd1a753fa7397fed753547f11ca8696646f2f3acb08e31016afac23e630c5d11f59f61fef57b0d2aa5";

/// Builder op-node libp2p peer ID (derived from SEQUENCER key for P2P identity).
/// This is used by the client op-node to connect to the builder op-node via P2P.
pub const BUILDER_LIBP2P_PEER_ID: &str = "16Uiu2HAkxp9nAsXsCthNWPkkpm4yG1eW7L4ENpVyzDZM8HE1yr12";

#[derive(Debug, Clone)]
/// Output of the L1 genesis generation.
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

#[derive(Debug, Clone)]
/// Output of the L2 contract deployment.
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

#[derive(Debug, Clone)]
/// A container for running devnet setup scripts.
pub struct SetupContainer {
    output_dir: PathBuf,
    chain_id: u64,
    l2_chain_id: u64,
    slot_duration: u64,
}

impl SetupContainer {
    /// Creates a new setup container with the given output directory.
    pub fn new(output_dir: impl Into<PathBuf>) -> Self {
        Self {
            output_dir: output_dir.into(),
            chain_id: 1337,
            l2_chain_id: 84538453,
            slot_duration: 2,
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

    /// Generates the L1 genesis files.
    pub fn generate_l1_genesis(&self) -> Result<L1GenesisOutput> {
        std::fs::create_dir_all(&self.output_dir).wrap_err("Failed to create output dir")?;
        std::fs::create_dir_all(self.output_dir.join("shared"))
            .wrap_err("Failed to create shared dir")?;

        self.ensure_setup_image_built()?;

        let output_mount = self.output_dir.to_string_lossy().to_string();
        let shared_mount = self.output_dir.join("shared").to_string_lossy().to_string();

        let _container = GenericImage::new("devnet-setup", "local")
            .with_wait_for(WaitFor::exit(ExitWaitStrategy::default().with_exit_code(0)))
            .with_env_var("OUTPUT_DIR", "/output")
            .with_env_var("SHARED_DIR", "/shared")
            .with_env_var("TEMPLATE_DIR", "/templates")
            .with_env_var("CHAIN_ID", self.chain_id.to_string())
            .with_env_var("SLOT_DURATION", self.slot_duration.to_string())
            .with_mount(Mount::bind_mount(output_mount, "/output"))
            .with_mount(Mount::bind_mount(shared_mount, "/shared"))
            .with_cmd(["setup-l1.sh"])
            .start()
            .wrap_err("Failed to run setup-l1.sh")?;

        ensure!(self.output_dir.join("cl/genesis.ssz").exists(), "genesis.ssz was not generated");

        Ok(L1GenesisOutput { output_dir: self.output_dir.clone() })
    }

    /// Deploys L2 contracts.
    pub fn deploy_l2_contracts(&self, l1_internal_rpc_url: &str) -> Result<L2DeploymentOutput> {
        self.ensure_setup_image_built()?;
        ensure_network_exists()?;

        std::fs::create_dir_all(self.output_dir.join("l2"))
            .wrap_err("Failed to create l2 output dir")?;

        let shared_mount = self.output_dir.join("shared").to_string_lossy().to_string();
        let l2_output_mount = self.output_dir.join("l2").to_string_lossy().to_string();

        let deployer_key = format!("0x{}", hex::encode(DEPLOYER.private_key.as_slice()));

        let image = GenericImage::new("devnet-setup", "local")
            .with_wait_for(WaitFor::exit(ExitWaitStrategy::default().with_exit_code(0)));

        let _container = image
            .with_network(network_name())
            .with_startup_timeout(Duration::from_secs(DEPLOY_TIMEOUT_SECS))
            .with_env_var("OUTPUT_DIR", "/output/l2")
            .with_env_var("SHARED_DIR", "/shared")
            .with_env_var("TEMPLATE_DIR", "/templates")
            .with_env_var("L1_RPC_URL", l1_internal_rpc_url)
            .with_env_var("L1_CHAIN_ID", self.chain_id.to_string())
            .with_env_var("L2_CHAIN_ID", self.l2_chain_id.to_string())
            .with_env_var("DEPLOYER_KEY", &deployer_key)
            .with_env_var("DEPLOYER_ADDR", format!("{:#x}", DEPLOYER.address))
            .with_env_var("SEQUENCER_ADDR", format!("{:#x}", SEQUENCER.address))
            .with_env_var("BATCHER_ADDR", format!("{:#x}", BATCHER.address))
            .with_env_var("PROPOSER_ADDR", format!("{:#x}", PROPOSER.address))
            .with_env_var("CHALLENGER_ADDR", format!("{:#x}", CHALLENGER.address))
            .with_env_var("BUILDER_P2P_KEY", BUILDER_P2P_KEY)
            .with_env_var("BUILDER_ENODE_ID", BUILDER_ENODE_ID)
            .with_mount(Mount::bind_mount(l2_output_mount, "/output/l2"))
            .with_mount(Mount::bind_mount(shared_mount, "/shared"))
            .with_cmd(["setup-l2.sh"])
            .start()
            .wrap_err("Failed to run setup-l2.sh")?;

        ensure!(
            self.output_dir.join("l2/genesis.json").exists(),
            "L2 genesis.json was not generated"
        );

        Ok(L2DeploymentOutput { output_dir: self.output_dir.clone() })
    }

    fn ensure_setup_image_built(&self) -> Result<()> {
        let image_exists = Command::new("docker")
            .args(["image", "inspect", SETUP_IMAGE_TAG])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);

        if image_exists {
            return Ok(());
        }

        let repo_root = self.find_repo_root()?;
        let dockerfile_path = repo_root.join("docker/Dockerfile.devnet");

        ensure!(dockerfile_path.exists(), "docker/Dockerfile.devnet not found");

        let status = Command::new("docker")
            .args(["build", "-t", SETUP_IMAGE_TAG, "-f", "docker/Dockerfile.devnet", "."])
            .current_dir(&repo_root)
            .status()
            .wrap_err("Failed to run docker build")?;

        ensure!(status.success(), "docker build failed");

        Ok(())
    }

    fn find_repo_root(&self) -> Result<PathBuf> {
        let mut path = std::env::current_dir()?;
        loop {
            if path.join("Cargo.toml").exists() && path.join("docker/Dockerfile.devnet").exists() {
                return Ok(path);
            }
            if !path.pop() {
                break;
            }
        }
        Err(eyre::eyre!("Could not find repository root with docker/Dockerfile.devnet"))
    }
}
