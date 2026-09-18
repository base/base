//! Inputs to offline genesis generation.

use std::{
    num::NonZeroU32,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use alloy_primitives::{Address, B256, U256, address};
use base_common_genesis::UpgradeConfig;
use clap::{Args, ValueEnum};
use eyre::{Result, ensure};
use serde::{Deserialize, Serialize};

/// Generate a complete offline Base development network.
#[derive(Debug, Args)]
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
    /// Number of mnemonic-derived beacon validators and matching keystores.
    #[arg(long, env = "BASE_DEVNET_VALIDATOR_COUNT", default_value = "1", value_parser = Self::parse_validator_count)]
    pub validator_count: NonZeroU32,
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
    /// Resolved development upgrade schedule.
    #[arg(skip)]
    pub upgrades: UpgradeConfig,
}

impl GenesisConfig {
    /// Parse a positive decimal count within the validator derivation index range.
    /// An empty environment override retains the default of one validator.
    pub fn parse_validator_count(value: &str) -> Result<NonZeroU32, String> {
        let value = if value.is_empty() { "1" } else { value };
        if value.bytes().all(|byte| byte.is_ascii_digit())
            && let Ok(count) = value.parse::<NonZeroU32>()
        {
            return Ok(count);
        }
        Err("BASE_DEVNET_VALIDATOR_COUNT must be a positive decimal integer no greater than 4294967295".into())
    }

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
        Ok(self)
    }
}

#[cfg(test)]
mod tests {
    use std::{env, process::Command as ProcessCommand};

    use clap::{Args, Command, FromArgMatches};

    use crate::GenesisCommand;

    #[test]
    fn validator_count_accepts_positive_decimal_overrides() {
        for (value, expected) in [("1", 1), ("64", 64), ("008", 8)] {
            let matches = GenesisCommand::augment_args(Command::new("genesis"))
                .try_get_matches_from(["genesis", "--validator-count", value])
                .unwrap();
            let command = GenesisCommand::from_arg_matches(&matches).unwrap();
            assert_eq!(command.config.validator_count.get(), expected);
        }
    }

    #[test]
    fn validator_count_rejects_invalid_overrides() {
        for value in [
            "0",
            "-1",
            "+1",
            "abc",
            "1.5",
            " 1",
            "1 ",
            "0x10",
            "4294967296",
            "18446744073709551616",
        ] {
            assert!(
                GenesisCommand::augment_args(Command::new("genesis"))
                    .try_get_matches_from(["genesis", &format!("--validator-count={value}")])
                    .is_err(),
                "accepted {value}"
            );
        }
    }

    #[test]
    fn validator_count_from_environment() {
        // Parse in child processes so environment overrides cannot race other tests.
        if let Ok(expected) = env::var("BASE_GENESIS_TEST_VALIDATOR_COUNT") {
            let result = GenesisCommand::augment_args(Command::new("genesis"))
                .try_get_matches_from(["genesis"]);
            if expected == "invalid" {
                let error = result.unwrap_err().to_string();
                assert!(error.contains("BASE_DEVNET_VALIDATOR_COUNT"), "{error}");
            } else {
                assert_eq!(
                    GenesisCommand::from_arg_matches(&result.unwrap())
                        .unwrap()
                        .config
                        .validator_count
                        .get(),
                    expected.parse::<u32>().unwrap()
                );
            }
            return;
        }
        for (value, expected) in [
            (None, "1"),
            (Some(""), "1"),
            (Some("64"), "64"),
            (Some("008"), "8"),
            (Some("0"), "invalid"),
            (Some("-1"), "invalid"),
            (Some("abc"), "invalid"),
            (Some("1.5"), "invalid"),
            (Some("18446744073709551616"), "invalid"),
        ] {
            let mut child = ProcessCommand::new(env::current_exe().unwrap());
            child
                .args(["--exact", "config::tests::validator_count_from_environment"])
                .env("BASE_GENESIS_TEST_VALIDATOR_COUNT", expected)
                .env_remove("BASE_DEVNET_VALIDATOR_COUNT");
            if let Some(value) = value {
                child.env("BASE_DEVNET_VALIDATOR_COUNT", value);
            }
            let output = child.output().unwrap();
            assert!(
                output.status.success(),
                "{value:?}: {} {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
}
