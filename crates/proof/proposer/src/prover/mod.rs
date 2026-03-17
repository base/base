//! Prover module for TEE-based block validation.
//!
//! This module provides the core prover functionality for generating
//! TEE-signed proposals for L2 block transitions by delegating to a
//! remote prover server via JSON-RPC.

mod types;
use std::{sync::Arc, time::Duration};

use alloy_consensus::Header;
use alloy_primitives::{B256, BloomInput};
use base_enclave::PerChainConfig;
use base_proof_primitives::{ProofRequest, ProofResult};
use base_proof_rpc::L2BlockRef;
use base_protocol::Predeploys;
pub use types::ProverProposal;
#[cfg(test)]
pub(crate) use types::test_helpers;

use crate::{error::ProposerError, prover_client::ProverClient};

/// Timeout for prover server RPC calls.
const PROVER_TIMEOUT: Duration = Duration::from_secs(600);

/// Prover for generating TEE-signed proposals via a remote prover server.
pub struct Prover {
    config_hash: B256,
    client: Arc<dyn ProverClient>,
}

impl Prover {
    #[must_use]
    pub fn new(mut config: PerChainConfig, client: Arc<dyn ProverClient>) -> Self {
        config.force_defaults();
        let config_hash = config.hash();
        Self { config_hash, client }
    }

    #[must_use]
    pub const fn config_hash(&self) -> B256 {
        self.config_hash
    }

    /// Proves a block by sending a `ProofRequest` to the prover server.
    pub async fn prove(
        &self,
        request: ProofRequest,
        from: L2BlockRef,
        to: L2BlockRef,
        has_withdrawals: bool,
    ) -> Result<ProverProposal, ProposerError> {
        let result = tokio::time::timeout(PROVER_TIMEOUT, self.client.prove(request))
            .await
            .map_err(|_| ProposerError::Prover("prover request timed out".into()))??;

        let proposal = match result {
            ProofResult::Tee { aggregate_proposal, .. } => aggregate_proposal,
            ProofResult::Zk { .. } => {
                return Err(ProposerError::Prover(
                    "unexpected ZK proof result from TEE prover".into(),
                ));
            }
        };

        Ok(ProverProposal { output: proposal, from, to, has_withdrawals })
    }
}

impl std::fmt::Debug for Prover {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Prover").field("config_hash", &self.config_hash).finish_non_exhaustive()
    }
}

/// Checks if a block has withdrawals by examining the logs bloom.
pub(crate) fn check_withdrawals(header: &Header) -> bool {
    header
        .logs_bloom
        .contains_input(BloomInput::Raw(Predeploys::L2_TO_L1_MESSAGE_PASSER.as_slice()))
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Bloom, BloomInput};

    use super::*;

    #[test]
    fn test_check_withdrawals_empty_bloom() {
        let header = Header { logs_bloom: Default::default(), ..Default::default() };
        assert!(!check_withdrawals(&header));
    }

    #[test]
    fn test_check_withdrawals_with_message_passer() {
        let mut bloom = Bloom::default();
        bloom.accrue(BloomInput::Raw(Predeploys::L2_TO_L1_MESSAGE_PASSER.as_slice()));
        let header = Header { logs_bloom: bloom, ..Default::default() };
        assert!(check_withdrawals(&header));
    }

    #[test]
    fn test_check_withdrawals_with_other_address() {
        let mut bloom = Bloom::default();
        let other_address =
            alloy_primitives::address!("0x1111111111111111111111111111111111111111");
        bloom.accrue(BloomInput::Raw(other_address.as_slice()));
        let header = Header { logs_bloom: bloom, ..Default::default() };
        assert!(!check_withdrawals(&header));
    }
}
