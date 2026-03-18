//! Prover module for TEE-based block validation.
//!
//! This module provides the core prover functionality for generating
//! TEE-signed proposals for L2 block transitions by delegating to a
//! remote prover server via JSON-RPC.

use std::sync::Arc;

use alloy_primitives::B256;
use base_enclave::PerChainConfig;
use base_proof_primitives::{ProofRequest, ProofResult};

use crate::{error::ProposerError, prover_client::ProverClient};

/// Prover for generating TEE-signed proposals via a remote prover server.
pub struct Prover {
    config_hash: B256,
    client: Arc<dyn ProverClient>,
}

impl Prover {
    /// Creates a new [`Prover`] from the given chain configuration and prover client.
    #[must_use]
    pub fn new(mut config: PerChainConfig, client: Arc<dyn ProverClient>) -> Self {
        config.force_defaults();
        let config_hash = config.hash();
        Self { config_hash, client }
    }

    /// Returns the configuration hash used for proposal verification.
    #[must_use]
    pub const fn config_hash(&self) -> B256 {
        self.config_hash
    }

    /// Proves a block range by sending a [`ProofRequest`] to the prover server.
    pub async fn prove(&self, request: ProofRequest) -> Result<ProofResult, ProposerError> {
        self.client.prove(request).await
    }
}

impl std::fmt::Debug for Prover {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Prover").field("config_hash", &self.config_hash).finish_non_exhaustive()
    }
}
