//! Inputs to offline genesis generation.

use std::{
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use alloy_primitives::{Address, B256, U256, address};
use base_common_genesis::BaseUpgrade;
use clap::{Args, Parser, ValueEnum};
use eyre::{Result, ensure};
use serde::{Deserialize, Serialize};

/// Generate a complete offline Base development network.
#[derive(Debug, Parser)]
pub struct GenesisCommand {
    /// Assembly stage; the complete Forge workflow is `just genesis`.
    #[arg(long, value_enum, default_value = "assemble")]
    pub stage: GenesisStage,
    /// Working directory shared by the stages of `just genesis`.
    #[arg(long)]
    pub work_dir: Option<PathBuf>,
    /// Directory containing the prepared contracts project and manifest.
    #[arg(long, env = "BASE_DEVNET_ARTIFACTS", default_value = "build/genesis")]
    pub artifacts_dir: PathBuf,
    /// Root directory for execution genesis, beacon state, and keys.
    #[arg(long, env = "OUTPUT_DIR", default_value = ".devnet/genesis")]
    pub output_dir: PathBuf,
    /// Chain generation parameters.
    #[command(flatten)]
    pub config: GenesisConfig,
}

/// Rust stages coordinated by the genesis shell workflow.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum GenesisStage {
    /// Resolve inputs and validate existing outputs before contract execution.
    Prepare,
    /// Compute the L2 output root for the final L1 deployment.
    Anchor,
    /// Assemble final execution and consensus files from exported allocations.
    Assemble,
}

/// Inputs affecting the generated chain state.
#[derive(Debug, Clone, PartialEq, Eq, Args, Serialize, Deserialize)]
pub struct GenesisConfig {
    /// L1 chain ID.
    #[arg(long, env = "CHAIN_ID", default_value_t = 1337)]
    pub l1_chain_id: u64,
    /// L2 chain ID.
    #[arg(long, env = "L2_CHAIN_ID", default_value_t = 84538453)]
    pub l2_chain_id: u64,
    /// Beacon slot duration in seconds.
    #[arg(long, env = "SLOT_DURATION", default_value_t = 12)]
    pub slot_duration: u64,
    /// Genesis Unix timestamp (defaults to the current time on first generation).
    #[arg(long, env = "BASE_DEVNET_TIMESTAMP")]
    pub timestamp: Option<u64>,
    /// Salt for deterministic contract deployment (random on first generation).
    #[arg(long, env = "BASE_DEVNET_SALT")]
    pub salt: Option<B256>,
    /// Owner of the development system contracts.
    #[arg(long, env = "DEPLOYER_ADDR", default_value_t = address!("f39fd6e51aad88f6f4ce6ab8827279cfffb92266"))]
    pub owner: Address,
    /// Sequencer address.
    #[arg(long, env = "SEQUENCER_ADDR", default_value_t = address!("70997970c51812dc3a010c7d01b50e0d17dc79c8"))]
    pub sequencer: Address,
    /// Batcher address.
    #[arg(long, env = "BATCHER_ADDR", default_value_t = address!("3c44cdddb6a900fa2b585dd299e03d12fa4293bc"))]
    pub batcher: Address,
    /// Proof proposer address.
    #[arg(long, env = "PROPOSER_ADDR", default_value_t = address!("90f79bf6eb2c4f870365e785982e1f101e93b906"))]
    pub proposer: Address,
    /// Proof challenger address.
    #[arg(long, env = "CHALLENGER_ADDR", default_value_t = address!("15d34aaf54267db7d7c367839aaf71a00a2c6a65"))]
    pub challenger: Address,
    /// Activation administrator (defaults to the sequencer).
    #[arg(long, env = "L2_ACTIVATION_ADMIN_ADDR")]
    pub activation_admin: Option<Address>,
    /// Wire the deployed `ProtocolVersions` proxy into the node configuration.
    #[arg(long, env = "UPGRADE_SIGNAL_PREINSTALL", default_value_t = true, action = clap::ArgAction::Set)]
    pub upgrade_signal: bool,
    /// Minimum packed protocol version imported into `ProtocolVersions`.
    #[arg(long, env = "UPGRADE_SIGNAL_MIN_PROTOCOL_VERSION", default_value = "4294967296")]
    pub minimum_protocol_version: U256,
    /// Optional development upgrade activation blocks.
    #[command(flatten)]
    pub upgrades: GenesisUpgrades,
}

/// Upgrade block overrides supported by the development network.
#[derive(Debug, Clone, Default, PartialEq, Eq, Args, Serialize, Deserialize)]
pub struct GenesisUpgrades {
    /// Isthmus activation block.
    #[arg(long = "isthmus-block")]
    pub isthmus: Option<u64>,
    /// Azul activation block.
    #[arg(long = "azul-block")]
    pub azul: Option<u64>,
    /// Beryl activation block.
    #[arg(long = "beryl-block")]
    pub beryl: Option<u64>,
    /// Cobalt activation block.
    #[arg(long = "cobalt-block")]
    pub cobalt: Option<u64>,
    /// Denim activation block.
    #[arg(long = "denim-block")]
    pub denim: Option<u64>,
    /// Zenith activation block.
    #[arg(long = "zenith-block")]
    pub zenith: Option<u64>,
}

impl GenesisConfig {
    /// Resolve fresh timestamp/salt inputs and validate the configuration.
    pub fn resolve(mut self) -> Result<Self> {
        self.timestamp =
            Some(self.timestamp.unwrap_or(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs()));
        self.salt = Some(self.salt.unwrap_or_else(B256::random));
        self.activation_admin = Some(self.activation_admin.unwrap_or(self.sequencer));
        ensure!(
            self.l1_chain_id > 0 && self.l2_chain_id > 0 && self.l1_chain_id != self.l2_chain_id,
            "chain IDs must be positive and distinct"
        );
        ensure!(self.slot_duration > 0, "slot duration must be positive");
        // Offline Solidity initialization runs at timestamp zero. Imported schedules must clear
        // SystemDeploy's two-hour notice buffer at that construction timestamp.
        ensure!(
            self.timestamp.unwrap_or_default() >= 7200,
            "genesis timestamp must be at least 7200"
        );
        for address in [
            self.owner,
            self.sequencer,
            self.batcher,
            self.proposer,
            self.challenger,
            self.activation_admin.unwrap_or_default(),
        ] {
            ensure!(!address.is_zero(), "role addresses must not be zero");
        }
        ensure!(
            self.minimum_protocol_version > U256::ZERO
                && self.minimum_protocol_version <= U256::from(u128::MAX),
            "minimum protocol version must fit a nonzero uint128"
        );
        for (name, block) in [
            ("L2_ISTHMUS_BLOCK", &mut self.upgrades.isthmus),
            ("L2_BASE_AZUL_BLOCK", &mut self.upgrades.azul),
            ("L2_BASE_BERYL_BLOCK", &mut self.upgrades.beryl),
            ("L2_BASE_COBALT_BLOCK", &mut self.upgrades.cobalt),
            ("L2_BASE_DENIM_BLOCK", &mut self.upgrades.denim),
            ("L2_BASE_ZENITH_BLOCK", &mut self.upgrades.zenith),
        ] {
            if block.is_none()
                && let Ok(value) = std::env::var(name)
                && !value.is_empty()
            {
                *block = Some(value.parse::<u64>().map_err(|_| eyre::eyre!("invalid {name}"))?);
            }
        }
        self.upgrades.timestamps(self.timestamp.unwrap_or_default())?;
        Ok(self)
    }
}

impl GenesisUpgrades {
    /// Resolve activation blocks to whole-second Unix timestamps.
    pub fn timestamps(&self, genesis: u64) -> Result<Vec<(BaseUpgrade, u64)>> {
        [
            (BaseUpgrade::Isthmus, self.isthmus),
            (BaseUpgrade::Azul, self.azul),
            (BaseUpgrade::Beryl, self.beryl),
            (BaseUpgrade::Cobalt, self.cobalt),
            (BaseUpgrade::Denim, self.denim),
            (BaseUpgrade::Zenith, self.zenith),
        ]
        .into_iter()
        .filter_map(|(name, block)| block.map(|block| (name, block)))
        .map(|(name, block)| {
            let elapsed = if let Some(denim) = self.denim.filter(|denim| block > *denim) {
                ensure!(
                    (block - denim).is_multiple_of(5),
                    "{name} must align to a whole second after Denim"
                );
                denim.checked_mul(2).and_then(|before| before.checked_add((block - denim) / 5))
            } else {
                block.checked_mul(2)
            };
            let timestamp = elapsed
                .and_then(|elapsed| genesis.checked_add(elapsed))
                .ok_or_else(|| eyre::eyre!("{name} activation timestamp overflow"))?;
            Ok((name, timestamp))
        })
        .collect()
    }
}

#[cfg(test)]
mod tests {
    use base_common_genesis::BaseUpgrade;

    use super::GenesisUpgrades;

    #[test]
    fn denim_changes_block_duration() {
        let upgrades = GenesisUpgrades { denim: Some(10), zenith: Some(15), ..Default::default() };
        assert_eq!(
            upgrades.timestamps(100).unwrap(),
            vec![(BaseUpgrade::Denim, 120), (BaseUpgrade::Zenith, 121)]
        );
    }

    #[test]
    fn rejects_fractional_seconds_and_overflow() {
        let upgrades = GenesisUpgrades { denim: Some(10), zenith: Some(11), ..Default::default() };
        assert!(upgrades.timestamps(100).is_err());
        assert!(
            GenesisUpgrades { isthmus: Some(u64::MAX), ..Default::default() }
                .timestamps(100)
                .is_err()
        );
    }
}
