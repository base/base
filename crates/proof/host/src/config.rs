use std::path::PathBuf;

use alloy_genesis::ChainConfig;
use alloy_provider::RootProvider;
use base_common_genesis::RollupConfig;
use base_common_network::Base;
use base_consensus_providers::{OnlineBeaconClient, OnlineBlobProvider};
use base_proof_primitives::ProofRequest;
use serde::Serialize;

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

/// Static infrastructure config — set once at startup, reused across proofs.
///
/// Constructed by the binary from CLI args or environment.
#[derive(Debug, Clone, Serialize)]
pub struct ProverConfig {
    /// L1 execution layer RPC URL.
    pub l1_eth_url: String,
    /// L2 execution layer RPC URL.
    pub l2_eth_url: String,
    /// L1 beacon API URL.
    pub l1_beacon_url: String,
    /// L2 chain ID.
    pub l2_chain_id: u64,
    /// Rollup configuration.
    pub rollup_config: RollupConfig,
    /// L1 chain configuration.
    pub l1_config: ChainConfig,
    /// Enables `debug_executePayload` for execution witness collection.
    pub enable_experimental_witness_endpoint: bool,
}

/// Configuration for the proof host.
#[derive(Debug, Clone)]
pub struct HostConfig {
    /// Per-proof parameters.
    pub request: ProofRequest,
    /// Static infrastructure config.
    pub prover: ProverConfig,
    /// Data directory for preimage data storage. When set, enables offline mode.
    pub data_dir: Option<PathBuf>,
}

impl HostConfig {
    /// Returns `true` if the host is running in offline mode.
    pub const fn is_offline(&self) -> bool {
        self.data_dir.is_some()
    }
}
