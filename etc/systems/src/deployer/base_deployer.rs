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

use super::{artifacts::DeploymentArtifacts, rollup_config::RollupConfigPatcher};
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

/// Container wrapper for L2 contract deployment via op-deployer.
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

    /// Runs op-deployer against the configured L1 and returns deployment artifacts.
    ///
    /// This is a blocking call that waits for op-deployer to finish. The deployment will
    /// only succeed when the L1 node is running and producing blocks.
    pub fn deploy(&self) -> Result<DeploymentArtifacts> {
        if DeploymentArtifacts::exists_in(&self.output_dir) {
            RollupConfigPatcher::patch_dir(&self.output_dir)
                .wrap_err("Existing deployment artifacts failed to patch")?;
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

        RollupConfigPatcher::patch_dir(&self.output_dir)
            .wrap_err("Failed to patch rollup config genesis hash")?;

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

WORKDIR=\"{WORKDIR}\"
OUTPUT_DIR=\"{OUTPUT_DIR}\"
INTENT_PATH=\"{INTENT_PATH}\"

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

L2_BLOCK_TIME_MS=\"${{L2_BLOCK_TIME_MS:-200}}\"
L2_GENESIS_TIME_SECONDS=$(jq -re '.genesis.l2_time' \"$OUTPUT_DIR/rollup.json\")
L2_GENESIS_TIME_MS=$((L2_GENESIS_TIME_SECONDS * 1000))
L2_GENESIS_TIME_MS_HEX=$(printf \"0x%x\" \"$L2_GENESIS_TIME_MS\")

TMP_ROLLUP=$(mktemp)
jq \
  --argjson block_time_ms \"$L2_BLOCK_TIME_MS\" \
  --argjson l2_genesis_time_ms \"$L2_GENESIS_TIME_MS\" \
  --argjson millis_per_second 1000 \
  'def to_millis: if . == null then null elif . == 0 then 0 else . * $millis_per_second end;
   def maybe_to_millis(path): if getpath(path) == null then . else setpath(path; getpath(path) | to_millis) end;
   .block_time = $block_time_ms
   | .genesis.l2_time = $l2_genesis_time_ms
   | maybe_to_millis([\"regolith_time\"])
   | maybe_to_millis([\"canyon_time\"])
   | maybe_to_millis([\"delta_time\"])
   | maybe_to_millis([\"ecotone_time\"])
   | maybe_to_millis([\"fjord_time\"])
   | maybe_to_millis([\"granite_time\"])
   | maybe_to_millis([\"holocene_time\"])
   | maybe_to_millis([\"pectra_blob_schedule_time\"])
   | maybe_to_millis([\"isthmus_time\"])
   | maybe_to_millis([\"jovian_time\"])' \
  \"$OUTPUT_DIR/rollup.json\" \
  > \"$TMP_ROLLUP\"
mv \"$TMP_ROLLUP\" \"$OUTPUT_DIR/rollup.json\"

TMP_GENESIS=$(mktemp)
jq \
  --arg timestamp \"$L2_GENESIS_TIME_MS_HEX\" \
  --argjson millis_per_second 1000 \
  'def to_millis: if . == null then null elif . == 0 then 0 else . * $millis_per_second end;
   def maybe_to_millis(path): if getpath(path) == null then . else setpath(path; getpath(path) | to_millis) end;
   .timestamp = $timestamp
   | maybe_to_millis([\"config\", \"regolithTime\"])
   | maybe_to_millis([\"config\", \"canyonTime\"])
   | maybe_to_millis([\"config\", \"deltaTime\"])
   | maybe_to_millis([\"config\", \"ecotoneTime\"])
   | maybe_to_millis([\"config\", \"fjordTime\"])
   | maybe_to_millis([\"config\", \"graniteTime\"])
   | maybe_to_millis([\"config\", \"holoceneTime\"])
   | maybe_to_millis([\"config\", \"isthmusTime\"])
   | maybe_to_millis([\"config\", \"jovianTime\"])
   | maybe_to_millis([\"config\", \"shanghaiTime\"])
   | maybe_to_millis([\"config\", \"cancunTime\"])
   | maybe_to_millis([\"config\", \"pragueTime\"])
   | maybe_to_millis([\"config\", \"osakaTime\"])' \
  \"$OUTPUT_DIR/genesis.json\" \
  > \"$TMP_GENESIS\"
mv \"$TMP_GENESIS\" \"$OUTPUT_DIR/genesis.json\"

op-deployer inspect l1 \
  --workdir \"$WORKDIR\" \
  \"$L2_CHAIN_ID\" \
  > \"$OUTPUT_DIR/l1-addresses.json\"
"#,
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
