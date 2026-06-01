//! Base-deployer container wrapper.

use std::path::{Path, PathBuf};

use alloy_primitives::{Address, B256};
use alloy_signer_local::PrivateKeySigner;
use eyre::{Result, WrapErr};
use testcontainers::{
    GenericImage, ImageExt,
    core::{Mount, WaitFor, wait::ExitWaitStrategy},
    runners::SyncRunner,
};
use url::Url;

use super::artifacts::DeploymentArtifacts;
use crate::config::{BATCHER, CHALLENGER, DEPLOYER, PROPOSER, SEQUENCER};

const OUTPUT_DIR: &str = "/output/l2";

/// Role address configuration for the deployment.
#[derive(Debug, Clone, Copy)]
pub struct RoleAddresses {
    /// Sequencer address.
    pub sequencer: Address,
    /// Batcher address.
    pub batcher: Address,
    /// Proposer address.
    pub proposer: Address,
    /// Challenger address.
    pub challenger: Address,
}

impl RoleAddresses {
    /// Creates a new role address bundle.
    pub const fn new(
        sequencer: Address,
        batcher: Address,
        proposer: Address,
        challenger: Address,
    ) -> Self {
        Self { sequencer, batcher, proposer, challenger }
    }
}

impl Default for RoleAddresses {
    fn default() -> Self {
        Self {
            sequencer: SEQUENCER.address,
            batcher: BATCHER.address,
            proposer: PROPOSER.address,
            challenger: CHALLENGER.address,
        }
    }
}

/// Container wrapper for L2 contract deployment via base-deployer.
///
/// Runs `setup-l2.sh` inside the `devnet-setup:local` Docker image to deploy
/// L2 contracts using forge scripts and collect deployment artifacts.
#[derive(Debug)]
pub struct DeployerContainer {
    l1_rpc_url: Url,
    l1_chain_id: u64,
    l2_chain_id: u64,
    deployer_private_key: B256,
    roles: RoleAddresses,
    output_dir: PathBuf,
    network: Option<String>,
}

impl DeployerContainer {
    /// Creates a new deployer container wrapper.
    pub fn new(
        l1_rpc_url: Url,
        l1_chain_id: u64,
        l2_chain_id: u64,
        deployer_private_key: B256,
        roles: RoleAddresses,
    ) -> Self {
        Self {
            l1_rpc_url,
            l1_chain_id,
            l2_chain_id,
            deployer_private_key,
            roles,
            output_dir: default_output_dir(),
            network: None,
        }
    }

    /// Overrides the output directory used for deployment artifacts.
    pub fn with_output_dir(mut self, output_dir: impl Into<PathBuf>) -> Self {
        self.output_dir = output_dir.into();
        self
    }

    /// Connects the container to the provided Docker network.
    pub fn with_network(mut self, network: impl Into<String>) -> Self {
        self.network = Some(network.into());
        self
    }

    /// Returns the host output directory for artifacts.
    pub fn output_dir(&self) -> &Path {
        &self.output_dir
    }

    /// Runs `setup-l2.sh` against the configured L1 and returns deployment artifacts.
    ///
    /// This is a blocking call that waits for the deployment to finish. The deployment
    /// will only succeed when the L1 node is running and producing blocks.
    pub fn deploy(&self) -> Result<DeploymentArtifacts> {
        if DeploymentArtifacts::exists_in(&self.output_dir) {
            return self.artifacts().wrap_err("existing deployment artifacts failed to load");
        }

        std::fs::create_dir_all(&self.output_dir).wrap_err("failed to create output directory")?;


        let image = GenericImage::new("devnet-setup", "local")
            .with_wait_for(WaitFor::exit(ExitWaitStrategy::default().with_exit_code(0)));

        let output_dir = self.output_dir.to_string_lossy().to_string();
        let mut request = image
            .with_entrypoint("/bin/bash")
            .with_cmd(["/usr/local/bin/setup-l2.sh"])
            .with_env_var("L1_RPC_URL", self.l1_rpc_url.to_string())
            .with_env_var("L1_CHAIN_ID", self.l1_chain_id.to_string())
            .with_env_var("L2_CHAIN_ID", self.l2_chain_id.to_string())
            .with_env_var("DEPLOYER_KEY", self.deployer_key_hex())
            .with_env_var("DEPLOYER_ADDR", format_address(self.deployer_address()?))
            .with_env_var("SEQUENCER_ADDR", format_address(self.roles.sequencer))
            .with_env_var("BATCHER_ADDR", format_address(self.roles.batcher))
            .with_env_var("PROPOSER_ADDR", format_address(self.roles.proposer))
            .with_env_var("CHALLENGER_ADDR", format_address(self.roles.challenger))
            .with_env_var("OUTPUT_DIR", OUTPUT_DIR)
            .with_env_var("TEMPLATE_DIR", "/templates")
            .with_mount(Mount::bind_mount(output_dir, OUTPUT_DIR));

        if let Some(network) = &self.network {
            request = request.with_network(network.clone());
        }

        let _container = request.start().wrap_err("failed to run base-deployer container")?;

        self.artifacts().wrap_err("failed to load deployment artifacts")
    }

    /// Loads deployment artifacts from the output directory.
    pub fn artifacts(&self) -> Result<DeploymentArtifacts> {
        DeploymentArtifacts::load_from_dir(&self.output_dir)
    }

    fn deployer_address(&self) -> Result<Address> {
        let signer = PrivateKeySigner::from_bytes(&self.deployer_private_key)
            .wrap_err("failed to derive deployer address from private key")?;
        Ok(signer.address())
    }

    fn deployer_key_hex(&self) -> String {
        format!("0x{}", hex::encode(self.deployer_private_key))
    }
}

fn format_address(address: Address) -> String {
    format!("{address:#x}")
}

fn default_output_dir() -> PathBuf {
    let suffix: u64 = rand::random();
    std::env::temp_dir().join(format!("base-deployer-{suffix}"))
}
