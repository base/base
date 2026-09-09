use std::{collections::BTreeMap, env, path::PathBuf};

use alloy_primitives::{Address, B256, U256};
use clap::Args;
use eyre::{Result, ensure};

use crate::{ContractArtifacts, GenesisConfig, GenesisOutput};

/// Generate the fixed, offline Base development network.
#[derive(Debug, Clone, Args)]
pub struct GenesisCommand {
    /// Already-built Base contract directory (prepare with `just contracts`).
    #[arg(long, env = "BASE_GENESIS_ARTIFACTS", default_value = ".contracts/artifacts")]
    pub artifacts: PathBuf,
    /// L1 configuration output directory.
    #[arg(long, env = "OUTPUT_DIR", default_value = "/output")]
    pub output_dir: PathBuf,
    /// L2 configuration output directory.
    #[arg(long, env = "L2_OUTPUT_DIR", default_value = "/devnet/l2/configs")]
    pub l2_output_dir: PathBuf,
    /// Shared output directory; defaults to the L1 output.
    #[arg(long, env = "SHARED_DIR")]
    pub shared_dir: Option<PathBuf>,
    /// L1 chain ID; also accepts `L1_CHAIN_ID` through the environment.
    #[arg(long, env = "CHAIN_ID")]
    pub l1_chain_id: Option<u64>,
    /// L2 chain ID.
    #[arg(long, env = "L2_CHAIN_ID", default_value_t = 84538453)]
    pub l2_chain_id: u64,
    /// Beacon slot duration in seconds.
    #[arg(long, env = "SLOT_DURATION", default_value_t = 12)]
    pub slot_duration: u64,
    /// Activation administrator; defaults to the sequencer.
    #[arg(long, env = "L2_ACTIVATION_ADMIN_ADDR")]
    pub activation_admin: Option<Address>,
    /// Isthmus activation block; empty means unspecified.
    #[arg(long, env = "L2_ISTHMUS_BLOCK")]
    pub isthmus_block: Option<String>,
    /// Azul activation block.
    #[arg(long, env = "L2_BASE_AZUL_BLOCK")]
    pub azul_block: Option<String>,
    /// Beryl activation block.
    #[arg(long, env = "L2_BASE_BERYL_BLOCK")]
    pub beryl_block: Option<String>,
    /// Cobalt activation block.
    #[arg(long, env = "L2_BASE_COBALT_BLOCK")]
    pub cobalt_block: Option<String>,
    /// Denim activation block.
    #[arg(long, env = "L2_BASE_DENIM_BLOCK")]
    pub denim_block: Option<String>,
    /// Genesis-only Zenith testing gate activation block.
    #[arg(long, env = "L2_BASE_ZENITH_BLOCK")]
    pub zenith_block: Option<String>,
}

impl GenesisCommand {
    /// Reads an optional nonempty environment setting without mutating the process environment.
    pub fn setting(key: &str) -> Option<String> {
        env::var(key).ok().filter(|v| !v.is_empty())
    }

    /// Resolves only the supported devnet inputs and generates the output files.
    pub fn run(self) -> Result<()> {
        let mut config = GenesisConfig::default();
        for (index, key) in
            ["DEPLOYER_ADDR", "SEQUENCER_ADDR", "BATCHER_ADDR", "PROPOSER_ADDR", "CHALLENGER_ADDR"]
                .iter()
                .enumerate()
        {
            if let Some(value) = Self::setting(key) {
                config.roles[index] = value.parse()?;
            }
        }
        config.l1_chain_id = match self.l1_chain_id {
            Some(id) => id,
            None => Self::setting("L1_CHAIN_ID").map(|v| v.parse()).transpose()?.unwrap_or(1337),
        };
        config.l2_chain_id = self.l2_chain_id;
        config.slot_duration = self.slot_duration;
        config.activation_admin = self.activation_admin.unwrap_or(config.roles[1]);
        config.timestamp = Self::setting("BASE_DEVNET_TIMESTAMP").map(|v| v.parse()).transpose()?;
        config.salt = Self::setting("BASE_DEVNET_SALT").map(|v| v.parse::<B256>()).transpose()?;
        if let Some(value) = Self::setting("UPGRADE_SIGNAL_PREINSTALL") {
            config.preinstall_upgrade_signal = value.parse()?;
        }
        if let Some(value) = Self::setting("UPGRADE_SIGNAL_MIN_PROTOCOL_VERSION") {
            config.minimum_version = value.parse::<U256>()?;
        }
        for (name, value) in [
            ("isthmus", self.isthmus_block),
            ("azul", self.azul_block),
            ("beryl", self.beryl_block),
            ("cobalt", self.cobalt_block),
            ("denim", self.denim_block),
            ("zenith", self.zenith_block),
        ] {
            if let Some(value) = value.filter(|v| !v.is_empty()) {
                ensure!(
                    value.bytes().all(|byte| byte.is_ascii_digit()),
                    "{name} activation block must be a non-negative integer"
                );
                config.upgrades.insert(name.to_owned(), value.parse()?);
            }
        }
        let mut output = GenesisOutput::new(self.output_dir);
        output.l2 = self.l2_output_dir;
        output.shared = self.shared_dir.unwrap_or_else(|| output.l1.clone());
        let peers: BTreeMap<String, [String; 2]> =
            serde_json::from_str(include_str!("../assets/peer-files.json"))?;
        for (file, [key, default]) in peers {
            output.peers.insert(file, format!("{}\n", Self::setting(&key).unwrap_or(default)));
        }
        output.upgrade_signal_env = format!(
            "BASE_NODE_UPGRADE_SIGNAL_L1_RPC=http://l1-el:{}\nBASE_NODE_UPGRADE_SIGNAL_MODE={}\nBASE_NODE_UPGRADE_SIGNAL_L1_BLOCK_TAG={}\n",
            Self::setting("L1_HTTP_PORT").unwrap_or_else(|| "4545".into()),
            Self::setting("UPGRADE_SIGNAL_MODE").unwrap_or_else(|| "runtime-admin".into()),
            Self::setting("UPGRADE_SIGNAL_L1_BLOCK_TAG").unwrap_or_else(|| "latest".into()),
        );
        output.generate(&config, &ContractArtifacts::load(&self.artifacts)?)
    }
}
