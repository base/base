use std::path::PathBuf;

use alloy_eips::BlockId;
use alloy_genesis::ChainConfig;
use alloy_primitives::{B256, Bytes};
use alloy_provider::RootProvider;
use async_trait::async_trait;
use base_common_genesis::RollupConfig;
use base_common_network::{Base, L1RpcProvider};
use base_consensus_derive::DynAltDaResolver;
use base_consensus_providers::L1BlobProvider;
use base_proof_primitives::ProofRequest;
use serde::Serialize;

/// The providers required for the host.
#[derive(Debug, Clone)]
pub struct HostProviders {
    /// The L1 EL provider.
    pub l1: L1RpcProvider,
    /// The L1 blob provider (beacon-backed, or calldata-only when no beacon is configured).
    pub blobs: L1BlobProvider,
    /// The L2 EL provider.
    pub l2: RootProvider<Base>,
    /// The L2 rollup RPC provider.
    pub l2_node: RootProvider,
    /// The alt-DA resolver, present only when a da-server URL is configured. Used to
    /// resolve `DERIVATION_VERSION_1` generic commitments into off-chain batch bytes.
    pub alt_da: Option<DynAltDaResolver>,
}

/// Supplies raw L1 data needed to populate preimage storage.
#[async_trait]
pub(crate) trait L1PreimageProvider {
    /// Fetches a raw L1 header by hash.
    async fn raw_header_by_hash(&self, hash: B256) -> crate::Result<Bytes>;

    /// Fetches a raw L1 header by block number.
    async fn raw_header_by_number(&self, block_number: u64) -> crate::Result<Bytes>;

    /// Fetches raw L1 receipts by block hash.
    async fn raw_receipts_by_hash(&self, hash: B256) -> crate::Result<Vec<Bytes>>;
}

#[async_trait]
impl L1PreimageProvider for L1RpcProvider {
    async fn raw_header_by_hash(&self, hash: B256) -> crate::Result<Bytes> {
        Ok(self.client().request("debug_getRawHeader", [hash]).await?)
    }

    async fn raw_header_by_number(&self, block_number: u64) -> crate::Result<Bytes> {
        Ok(self.client().request("debug_getRawHeader", (BlockId::number(block_number),)).await?)
    }

    async fn raw_receipts_by_hash(&self, hash: B256) -> crate::Result<Vec<Bytes>> {
        Ok(self.client().request("debug_getRawReceipts", [hash]).await?)
    }
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
    /// L2 rollup RPC URL.
    pub l2_node_url: String,
    /// L1 beacon API URL, or `None` when the L1 parent has no beacon/blob DA endpoint.
    pub l1_beacon_url: Option<String>,
    /// Alt-DA (da-server) URL, or `None` when the chain uses inline calldata/blob DA.
    /// When set, the host resolves `DERIVATION_VERSION_1` generic commitments against
    /// this server and the client derives from off-chain DA.
    pub da_server_url: Option<String>,
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
