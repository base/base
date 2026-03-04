use std::path::PathBuf;

use alloy_primitives::B256;
use alloy_provider::RootProvider;
use base_alloy_network::Base;
use base_consensus_genesis::{L1ChainConfig, RollupConfig};
use base_consensus_providers::{OnlineBeaconClient, OnlineBlobProvider};
use serde::Serialize;

use crate::{HostError, Result};

/// The providers required for the host.
#[derive(Debug, Clone)]
pub struct HostProviders {
    /// The L1 EL provider.
    pub l1: RootProvider,
    /// The L1 beacon node provider.
    pub blobs: OnlineBlobProvider<OnlineBeaconClient>,
    /// The L2 EL provider.
    pub l2: RootProvider<Base>,
}

/// Configuration for the proof host.
#[derive(Default, Serialize, Clone, Debug)]
pub struct HostConfig {
    /// Hash of the L1 head block.
    pub l1_head: B256,
    /// Hash of the agreed upon safe L2 block.
    pub agreed_l2_head_hash: B256,
    /// Agreed safe L2 Output Root to start derivation from.
    pub agreed_l2_output_root: B256,
    /// Claimed L2 output root to validate.
    pub claimed_l2_output_root: B256,
    /// Number of the L2 block that the claimed output root commits to.
    pub claimed_l2_block_number: u64,
    /// Address of L2 JSON-RPC endpoint to use.
    pub l2_node_address: Option<String>,
    /// Address of L1 JSON-RPC endpoint to use.
    pub l1_node_address: Option<String>,
    /// Address of the L1 Beacon API endpoint to use.
    pub l1_beacon_address: Option<String>,
    /// Data directory for preimage data storage.
    pub data_dir: Option<PathBuf>,
    /// The L2 chain ID of a supported chain.
    pub l2_chain_id: Option<u64>,
    /// Path to rollup config.
    pub rollup_config_path: Option<PathBuf>,
    /// Path to L1 config.
    pub l1_config_path: Option<PathBuf>,
    /// Enables the use of `debug_executePayload` to collect the execution witness.
    pub enable_experimental_witness_endpoint: bool,
}

impl HostConfig {
    /// Returns `true` if the host is running in offline mode.
    pub const fn is_offline(&self) -> bool {
        self.l1_node_address.is_none()
            && self.l2_node_address.is_none()
            && self.l1_beacon_address.is_none()
            && self.data_dir.is_some()
    }

    /// Reads the [`RollupConfig`] from the file system.
    pub fn read_rollup_config(&self) -> Result<RollupConfig> {
        let path = self.rollup_config_path.as_ref().ok_or(HostError::NoRollupConfig)?;
        let ser_config = std::fs::read_to_string(path)?;
        serde_json::from_str(&ser_config).map_err(HostError::from)
    }

    /// Reads the [`L1ChainConfig`] from the file system.
    pub fn read_l1_config(&self) -> Result<L1ChainConfig> {
        let path = self.l1_config_path.as_ref().ok_or(HostError::NoL1Config)?;
        let ser_config = std::fs::read_to_string(path)?;
        serde_json::from_str(&ser_config).map_err(HostError::from)
    }
}
