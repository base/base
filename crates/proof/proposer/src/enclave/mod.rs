//! Enclave client types for TEE proof generation.

use async_trait::async_trait;
use base_enclave::{AggregateRequest, RollupConfig};
pub use base_enclave::{PerChainConfig, Proposal};
use base_enclave_client::ClientError;
pub use base_enclave_client::EnclaveClient;
use base_tee_prover::TeeExecutor;

use crate::ProposerError;

/// Trait abstracting the enclave RPC client for testability.
///
/// Extends [`TeeExecutor`] (which provides `execute_stateless`) with the
/// `aggregate` method needed only by the proposer.
#[async_trait]
pub trait EnclaveClientTrait: TeeExecutor {
    /// Aggregates multiple proposals into a single batched proposal.
    async fn aggregate(&self, request: AggregateRequest) -> Result<Proposal, ClientError>;
}

#[async_trait]
impl EnclaveClientTrait for EnclaveClient {
    async fn aggregate(&self, request: AggregateRequest) -> Result<Proposal, ClientError> {
        self.aggregate(request).await
    }
}

/// Convert a [`RollupConfig`] (from RPC) to [`PerChainConfig`].
///
/// This is a thin wrapper around [`base_tee_prover::ConfigBuilder::rollup_config_to_per_chain_config`]
/// that maps the error to [`ProposerError`].
///
/// # Errors
///
/// Returns an error if the deposit contract address or L1 system config address
/// cannot be parsed from the hex string.
pub fn rollup_config_to_per_chain_config(
    cfg: &RollupConfig,
) -> Result<PerChainConfig, ProposerError> {
    base_tee_prover::ConfigBuilder::rollup_config_to_per_chain_config(cfg)
        .map_err(|e| ProposerError::Config(e.to_string()))
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
