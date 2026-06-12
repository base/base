//! Proof submission types for prover-service worker delivery.
//!
//! [`ProofSubmitter`] is backend-neutral: hosts build a
//! `WorkerSubmitProofRequest` from their own proof result type, then hand the
//! request to this shared worker component for delivery.

use std::time::Duration;

use base_prover_service_client::ProverServiceClientError;
use base_retry::{
    DEFAULT_UNBOUNDED_INITIAL_DELAY, DEFAULT_UNBOUNDED_MAX_DELAY, MIN_RETRY_DELAY, RetryConfig,
};
use thiserror::Error;

/// Minimum delay used to avoid tight retry loops.
pub const MIN_PROOF_SUBMITTER_BACKOFF: Duration = MIN_RETRY_DELAY;

/// Default initial retry delay for proof submission.
pub const DEFAULT_PROOF_SUBMITTER_INITIAL_BACKOFF: Duration = DEFAULT_UNBOUNDED_INITIAL_DELAY;

/// Default maximum retry delay for proof submission.
pub const DEFAULT_PROOF_SUBMITTER_MAX_BACKOFF: Duration = DEFAULT_UNBOUNDED_MAX_DELAY;

/// Exponential backoff configuration for proof submission retries.
pub type ProofSubmitterBackoffConfig = RetryConfig;

/// Errors raised while preparing or submitting a generated proof.
#[derive(Debug, Error)]
pub enum ProofSubmitterError {
    /// The generated proof result is not one this worker can submit.
    #[error("proof submitter received an unsupported proof result")]
    UnsupportedProofResult,
    /// Prover service worker API submission failed.
    #[error(transparent)]
    Submit(#[from] ProverServiceClientError),
}

impl ProofSubmitterError {
    /// Returns `true` when retrying the submission may succeed.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::UnsupportedProofResult => false,
            Self::Submit(error) => error.is_retryable(),
        }
    }
}

/// Submitter for delivering generated proofs to the prover-service worker API.
#[derive(Clone, Debug)]
pub struct ProofSubmitter<Client> {
    client: Client,
    backoff: ProofSubmitterBackoffConfig,
}

impl<Client> ProofSubmitter<Client> {
    /// Creates a proof submitter using the default backoff config.
    pub const fn new(client: Client) -> Self {
        Self {
            client,
            backoff: ProofSubmitterBackoffConfig::unbounded(
                DEFAULT_PROOF_SUBMITTER_INITIAL_BACKOFF,
                DEFAULT_PROOF_SUBMITTER_MAX_BACKOFF,
            ),
        }
    }

    /// Sets the retry backoff config.
    pub const fn with_backoff_config(mut self, backoff: ProofSubmitterBackoffConfig) -> Self {
        self.backoff = backoff;
        self
    }

    /// Returns the configured retry backoff.
    pub const fn backoff_config(&self) -> ProofSubmitterBackoffConfig {
        self.backoff
    }

    /// Returns the underlying worker client.
    pub const fn client(&self) -> &Client {
        &self.client
    }
}
