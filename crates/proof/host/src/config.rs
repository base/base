use std::path::PathBuf;

use alloy_genesis::ChainConfig;
use alloy_provider::RootProvider;
use base_common_genesis::RollupConfig;
use base_common_network::Base;
use base_consensus_providers::{OnlineBeaconClient, OnlineBlobProvider};
use base_proof_contracts::ProtocolVersionsContractClient;
use base_proof_primitives::ProofRequest;
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

    /// Resolves the full `ProtocolVersions` schedule required by this proof request.
    pub async fn resolve_protocol_versions_schedule(
        mut self,
        l1_provider: &RootProvider,
    ) -> Result<Self> {
        if self.request.activation_schedule_hash == alloy_primitives::B256::ZERO {
            return Ok(self);
        }

        let computed_hash = self
            .request
            .protocol_versions_schedule
            .compute_schedule_hash(&self.prover.rollup_config)
            .map_err(|error| HostError::Custom(format!("invalid ProtocolVersions schedule: {error}")))?;
        if computed_hash == self.request.activation_schedule_hash {
            return Ok(self);
        }

        if !self.request.protocol_versions_schedule.is_empty() {
            return Err(HostError::Custom(format!(
                "ProtocolVersions schedule hash mismatch: expected {}, got {}",
                self.request.activation_schedule_hash, computed_hash,
            )));
        }

        let protocol_versions_client = ProtocolVersionsContractClient::from_provider(l1_provider.clone());
        self.request.protocol_versions_schedule = protocol_versions_client
            .schedule_for_hash(
                self.prover.rollup_config.protocol_versions_address,
                self.request.activation_schedule_hash,
            )
            .await
            .map_err(|error| {
                HostError::Custom(format!("failed to resolve ProtocolVersions schedule: {error}"))
            })?;

        let recomputed_hash = self
            .request
            .protocol_versions_schedule
            .compute_schedule_hash(&self.prover.rollup_config)
            .map_err(|error| HostError::Custom(format!("invalid ProtocolVersions schedule: {error}")))?;
        if recomputed_hash != self.request.activation_schedule_hash {
            return Err(HostError::Custom(format!(
                "resolved ProtocolVersions schedule hash mismatch: expected {}, got {}",
                self.request.activation_schedule_hash, recomputed_hash,
            )));
        }

        Ok(self)
    }
}
