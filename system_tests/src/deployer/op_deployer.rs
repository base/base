//! op-deployer container wrapper.

use std::path::{Path, PathBuf};

use alloy_primitives::{Address, B256};
use alloy_signer_local::PrivateKeySigner;
use eyre::{Result, WrapErr, eyre};
use testcontainers::{
    GenericImage, ImageExt,
    core::{Mount, WaitFor, wait::ExitWaitStrategy},
    runners::SyncRunner,
};
use url::Url;

use super::artifacts::DeploymentArtifacts;
use crate::{
    config::{self, BATCHER, CHALLENGER, DEPLOYER, PROPOSER, SEQUENCER},
    images::OP_DEPLOYER_IMAGE,
};

const OUTPUT_DIR: &str = "/output";
const WORKDIR: &str = "/op-deployer";
const INTENT_PATH: &str = "/config/intent.toml";

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

/// op-deployer container wrapper for L2 contract deployment.
#[derive(Debug)]
pub struct OpDeployerContainer {
    l1_rpc_url: Url,
    l1_chain_id: u64,
    l2_chain_id: u64,
    deployer_private_key: B256,
    roles: RoleAddresses,
    output_dir: PathBuf,
    network: Option<String>,
}

impl OpDeployerContainer {
    /// Creates a new op-deployer container wrapper.
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

    /// Runs op-deployer against the configured L1 and returns deployment artifacts.
    ///
    /// This is a blocking call that waits for op-deployer to finish. The deployment will
    /// only succeed when the L1 node is running and producing blocks.
    pub fn deploy(&self) -> Result<DeploymentArtifacts> {
        if DeploymentArtifacts::exists_in(&self.output_dir) {
            return self.artifacts().wrap_err("Existing deployment artifacts failed to load");
        }

        std::fs::create_dir_all(&self.output_dir).wrap_err("Failed to create output directory")?;

        let intent_toml = self.intent_toml()?;
        let script = deploy_script();
        let (image_name, image_tag) = OP_DEPLOYER_IMAGE
            .rsplit_once(':')
            .ok_or_else(|| eyre!("op-deployer image tag is missing"))?;

        let image = GenericImage::new(image_name, image_tag)
            .with_entrypoint("sh")
            .with_wait_for(WaitFor::exit(ExitWaitStrategy::default().with_exit_code(0)));

        let cmd = vec!["-c".to_string(), script];
        let output_dir = self.output_dir.to_string_lossy().to_string();
        let mut request = image
            .with_cmd(cmd)
            .with_env_var("L1_RPC_URL", self.l1_rpc_url.to_string())
            .with_env_var("L1_CHAIN_ID", self.l1_chain_id.to_string())
            .with_env_var("L2_CHAIN_ID", self.l2_chain_id.to_string())
            .with_env_var("DEPLOYER_KEY", self.deployer_key_hex())
            .with_copy_to(INTENT_PATH, intent_toml.into_bytes())
            .with_mount(Mount::bind_mount(output_dir, OUTPUT_DIR));

        if let Some(network) = &self.network {
            request = request.with_network(network.clone());
        }

        let _container = request.start().wrap_err("Failed to run op-deployer container")?;

        self.artifacts().wrap_err("Failed to load deployment artifacts")
    }

    /// Loads deployment artifacts from the output directory.
    pub fn artifacts(&self) -> Result<DeploymentArtifacts> {
        DeploymentArtifacts::load_from_dir(&self.output_dir)
    }

    fn intent_toml(&self) -> Result<String> {
        let mut intent = config::l2_intent_toml(self.l1_chain_id, self.l2_chain_id);
        let deployer_address = self.deployer_address()?;

        intent = replace_address(intent, DEPLOYER.address, deployer_address);
        intent = replace_address(intent, SEQUENCER.address, self.roles.sequencer);
        intent = replace_address(intent, BATCHER.address, self.roles.batcher);
        intent = replace_address(intent, PROPOSER.address, self.roles.proposer);
        intent = replace_address(intent, CHALLENGER.address, self.roles.challenger);

        Ok(intent)
    }

    fn deployer_address(&self) -> Result<Address> {
        let signer = PrivateKeySigner::from_bytes(&self.deployer_private_key)
            .wrap_err("Failed to derive deployer address from private key")?;
        Ok(signer.address())
    }

    fn deployer_key_hex(&self) -> String {
        format!("0x{}", hex::encode(self.deployer_private_key))
    }
}

fn deploy_script() -> String {
    format!(
        r#"set -e

WORKDIR=\"{workdir}\"
OUTPUT_DIR=\"{output_dir}\"
INTENT_PATH=\"{intent_path}\"

mkdir -p \"$WORKDIR\" \"$OUTPUT_DIR\"

if [ -f \"$OUTPUT_DIR/genesis.json\" ] && [ -f \"$OUTPUT_DIR/rollup.json\" ] && [ -f \"$OUTPUT_DIR/l1-addresses.json\" ]; then
  echo \"Deployment artifacts already exist, skipping op-deployer\"
  exit 0
fi

op-deployer init \
  --l1-chain-id \"$L1_CHAIN_ID\" \
  --l2-chain-ids \"$L2_CHAIN_ID\" \
  --intent-type custom \
  --workdir \"$WORKDIR\"

cp \"$INTENT_PATH\" \"$WORKDIR/intent.toml\"

op-deployer apply \
  --workdir \"$WORKDIR\" \
  --deployment-target live \
  --l1-rpc-url \"$L1_RPC_URL\" \
  --private-key \"$DEPLOYER_KEY\"

op-deployer inspect genesis \
  --workdir \"$WORKDIR\" \
  \"$L2_CHAIN_ID\" \
  > \"$OUTPUT_DIR/genesis.json\"

op-deployer inspect rollup \
  --workdir \"$WORKDIR\" \
  \"$L2_CHAIN_ID\" \
  > \"$OUTPUT_DIR/rollup.json\"

op-deployer inspect l1 \
  --workdir \"$WORKDIR\" \
  \"$L2_CHAIN_ID\" \
  > \"$OUTPUT_DIR/l1-addresses.json\"
"#,
        workdir = WORKDIR,
        output_dir = OUTPUT_DIR,
        intent_path = INTENT_PATH,
    )
}

fn replace_address(input: String, from: Address, to: Address) -> String {
    input.replace(&format_address(from), &format_address(to))
}

fn format_address(address: Address) -> String {
    format!("{address:#x}")
}

fn default_output_dir() -> PathBuf {
    let suffix: u64 = rand::random();
    std::env::temp_dir().join(format!("op-deployer-{suffix}"))
}
