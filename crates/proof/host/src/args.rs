//! CLI arguments for the host.

use std::path::PathBuf;

use alloy_primitives::B256;
use base_consensus_genesis::{L1ChainConfig, RollupConfig};
use clap::Parser;
use serde::Serialize;

use crate::HostError;

/// Arguments for the host.
#[derive(Default, Parser, Serialize, Clone, Debug)]
pub struct HostArgs {
    /// Hash of the L1 head block. Derivation stops after this block is processed.
    #[arg(long, value_name = "L1_HEAD")]
    pub l1_head: B256,
    /// Hash of the agreed upon safe L2 block committed to by `--agreed-l2-output-root`.
    #[arg(long, value_name = "AGREED_L2_HEAD_HASH")]
    pub agreed_l2_head_hash: B256,
    /// Agreed safe L2 Output Root to start derivation from.
    #[arg(long, value_name = "AGREED_L2_OUTPUT_ROOT")]
    pub agreed_l2_output_root: B256,
    /// Claimed L2 output root at block # `--claimed-l2-block-number` to validate.
    #[arg(long, value_name = "CLAIMED_L2_OUTPUT_ROOT")]
    pub claimed_l2_output_root: B256,
    /// Number of the L2 block that the claimed output root commits to.
    #[arg(long, value_name = "CLAIMED_L2_BLOCK_NUMBER")]
    pub claimed_l2_block_number: u64,
    /// Address of L2 JSON-RPC endpoint to use (eth and debug namespace required).
    #[arg(long, value_name = "L2_NODE_ADDRESS")]
    pub l2_node_address: Option<String>,
    /// Address of L1 JSON-RPC endpoint to use (eth and debug namespace required).
    #[arg(long, value_name = "L1_NODE_ADDRESS")]
    pub l1_node_address: Option<String>,
    /// Address of the L1 Beacon API endpoint to use.
    #[arg(long, value_name = "L1_BEACON_ADDRESS")]
    pub l1_beacon_address: Option<String>,
    /// The Data Directory for preimage data storage. Optional if running in online mode,
    /// required if running in offline mode.
    #[arg(long, value_name = "DATA_DIR")]
    pub data_dir: Option<PathBuf>,
    /// Run the client program natively.
    #[arg(long)]
    pub native: bool,
    /// Run in pre-image server mode without executing any client program.
    #[arg(long)]
    pub server: bool,
    /// The L2 chain ID of a supported chain.
    #[arg(long, value_name = "L2_CHAIN_ID")]
    pub l2_chain_id: Option<u64>,
    /// Path to rollup config.
    #[arg(long, value_name = "ROLLUP_CONFIG_PATH")]
    pub rollup_config_path: Option<PathBuf>,
    /// Path to L1 config.
    #[arg(long, value_name = "L1_CONFIG_PATH")]
    pub l1_config_path: Option<PathBuf>,
    /// Enables the use of `debug_executePayload` to collect the execution witness.
    #[arg(long)]
    pub enable_experimental_witness_endpoint: bool,
}

impl HostArgs {
    /// Returns `true` if the host is running in offline mode.
    pub const fn is_offline(&self) -> bool {
        self.l1_node_address.is_none()
            && self.l2_node_address.is_none()
            && self.l1_beacon_address.is_none()
            && self.data_dir.is_some()
    }

    /// Reads the [`RollupConfig`] from the file system.
    pub fn read_rollup_config(&self) -> Result<RollupConfig, HostError> {
        let path = self.rollup_config_path.as_ref().ok_or_else(|| HostError::NoRollupConfigPath)?;

        let ser_config = std::fs::read_to_string(path)?;
        serde_json::from_str(&ser_config).map_err(HostError::SerdeJson)
    }

    /// Reads the [`L1ChainConfig`] from the file system.
    pub fn read_l1_config(&self) -> Result<L1ChainConfig, HostError> {
        let path = self.l1_config_path.as_ref().ok_or_else(|| HostError::NoL1ConfigPath)?;

        let ser_config = std::fs::read_to_string(path)?;
        serde_json::from_str(&ser_config).map_err(HostError::SerdeJson)
    }
}
