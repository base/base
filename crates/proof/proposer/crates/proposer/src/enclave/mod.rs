//! Enclave client types for TEE proof generation.

use alloy_primitives::{Address, B256, U256};
use async_trait::async_trait;

pub use op_enclave_client::EnclaveClient;
use op_enclave_client::{ClientError, ExecuteStatelessRequest};
use op_enclave_core::types::config::{BlockId, Genesis, GenesisSystemConfig, RollupConfig};
pub use op_enclave_core::{Proposal, executor::ExecutionWitness, types::config::PerChainConfig};

use crate::ProposerError;

/// Trait abstracting the enclave RPC client for testability.
///
/// This follows the same pattern as [`crate::rpc::L1Client`] and [`crate::rpc::L2Client`].
#[async_trait]
pub trait EnclaveClientTrait: Send + Sync {
    /// Executes stateless block validation in the enclave.
    async fn execute_stateless(
        &self,
        req: ExecuteStatelessRequest,
    ) -> Result<Proposal, ClientError>;

    /// Aggregates multiple proposals into a single batched proposal.
    async fn aggregate(
        &self,
        config_hash: B256,
        prev_output_root: B256,
        proposals: Vec<Proposal>,
    ) -> Result<Proposal, ClientError>;
}

#[async_trait]
impl EnclaveClientTrait for EnclaveClient {
    async fn execute_stateless(
        &self,
        req: ExecuteStatelessRequest,
    ) -> Result<Proposal, ClientError> {
        self.execute_stateless(req).await
    }

    async fn aggregate(
        &self,
        config_hash: B256,
        prev_output_root: B256,
        proposals: Vec<Proposal>,
    ) -> Result<Proposal, ClientError> {
        self.aggregate(config_hash, prev_output_root, proposals)
            .await
    }
}

/// Convert a [`RollupConfig`] (from RPC) to [`PerChainConfig`].
///
/// This is useful when you have a rollup config from an op-node RPC and need
/// to create a [`PerChainConfig`] for the prover.
///
/// # Errors
///
/// Returns an error if the deposit contract address or L1 system config address
/// cannot be parsed from the hex string.
pub fn rollup_config_to_per_chain_config(
    cfg: &RollupConfig,
) -> Result<PerChainConfig, ProposerError> {
    let deposit_contract_address = cfg.deposit_contract_address;
    let l1_system_config_address = cfg.l1_system_config_address;

    let (batcher_addr, scalar, gas_limit) =
        cfg.genesis
            .system_config
            .as_ref()
            .map_or((Address::ZERO, B256::ZERO, 30_000_000), |sc| {
                (
                    sc.batcher_address,
                    B256::from(sc.scalar.to_be_bytes::<32>()),
                    sc.gas_limit,
                )
            });

    Ok(PerChainConfig {
        chain_id: U256::from(cfg.l2_chain_id.id()),
        genesis: Genesis {
            l1: BlockId {
                hash: cfg.genesis.l1.hash,
                number: cfg.genesis.l1.number,
            },
            l2: BlockId {
                hash: cfg.genesis.l2.hash,
                number: cfg.genesis.l2.number,
            },
            l2_time: cfg.genesis.l2_time,
            system_config: GenesisSystemConfig {
                batcher_addr,
                overhead: B256::ZERO,
                scalar,
                gas_limit,
            },
        },
        block_time: cfg.block_time,
        deposit_contract_address,
        l1_system_config_address,
    })
}

/// Creates an enclave client with the given configuration.
///
/// # Arguments
///
/// * `url` - The URL of the enclave RPC server
/// * `skip_tls_verify` - Whether to skip TLS certificate verification
///
/// # Errors
///
/// Returns an error if the client cannot be created.
///
/// # Warning
///
/// Setting `skip_tls_verify` to `true` disables TLS certificate verification,
/// making the connection vulnerable to man-in-the-middle attacks.
/// This should ONLY be used in development/testing environments.
pub fn create_enclave_client(
    url: &str,
    skip_tls_verify: bool,
) -> Result<EnclaveClient, ProposerError> {
    if skip_tls_verify {
        tracing::warn!("TLS certificate verification is disabled for enclave RPC connection");
    }

    EnclaveClient::with_tls_config(url, 10 * 1024 * 1024, skip_tls_verify)
        .map_err(|e| ProposerError::Enclave(e.to_string()))
}
