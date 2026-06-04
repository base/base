//! Retrying proof requester provider.

use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use backon::{ExponentialBuilder, Retryable};
use base_prover_service_protocol::{
    GetProofRequest, GetProofResponse, ListProofsRequest, ListProofsResponse,
    ProveBlockRangeRequest, ProveBlockRangeResponse,
};
use tracing::warn;

use crate::{ProofRequesterProvider, ProverServiceClientError};

/// Minimum delay used to avoid tight retry loops.
pub const MIN_PROOF_REQUESTER_RETRY_DELAY: Duration = Duration::from_millis(1);

/// Default maximum retry attempts for requester RPC operations.
pub const DEFAULT_PROOF_REQUESTER_MAX_ATTEMPTS: u32 = 5;

/// Default initial retry delay for requester RPC operations.
pub const DEFAULT_PROOF_REQUESTER_INITIAL_DELAY: Duration = Duration::from_millis(100);

/// Default maximum retry delay for requester RPC operations.
pub const DEFAULT_PROOF_REQUESTER_MAX_DELAY: Duration = Duration::from_secs(10);

/// Exponential backoff configuration for proof requester retries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProofRequesterRetryConfig {
    /// Maximum number of retry attempts.
    pub max_attempts: u32,
    /// First delay after a retryable requester failure.
    pub initial_delay: Duration,
    /// Maximum delay between retry attempts.
    pub max_delay: Duration,
}

impl ProofRequesterRetryConfig {
    /// Creates a proof requester retry config.
    pub const fn new(max_attempts: u32, initial_delay: Duration, max_delay: Duration) -> Self {
        Self { max_attempts, initial_delay, max_delay }
    }

    /// Returns the configured max attempts, clamped to at least one attempt.
    pub const fn normalized_max_attempts(&self) -> u32 {
        if self.max_attempts == 0 { 1 } else { self.max_attempts }
    }

    /// Returns the configured max delay, clamped to the minimum allowed delay.
    pub fn normalized_max_delay(&self) -> Duration {
        self.max_delay.max(MIN_PROOF_REQUESTER_RETRY_DELAY)
    }

    /// Returns the configured initial delay, clamped to the configured max delay.
    pub fn normalized_initial_delay(&self) -> Duration {
        self.initial_delay.max(MIN_PROOF_REQUESTER_RETRY_DELAY).min(self.normalized_max_delay())
    }

    /// Creates a `backon` [`ExponentialBuilder`] from this configuration.
    pub fn to_backoff_builder(&self) -> ExponentialBuilder {
        ExponentialBuilder::default()
            .with_min_delay(self.normalized_initial_delay())
            .with_max_delay(self.normalized_max_delay())
            .with_max_times(self.normalized_max_attempts() as usize)
            .with_jitter()
    }
}

impl Default for ProofRequesterRetryConfig {
    fn default() -> Self {
        Self::new(
            DEFAULT_PROOF_REQUESTER_MAX_ATTEMPTS,
            DEFAULT_PROOF_REQUESTER_INITIAL_DELAY,
            DEFAULT_PROOF_REQUESTER_MAX_DELAY,
        )
    }
}

/// Proof requester wrapper that retries transient requester RPC failures.
#[derive(Clone)]
pub struct RetryingProofRequester {
    inner: Arc<dyn ProofRequesterProvider>,
    retry: ProofRequesterRetryConfig,
}

impl RetryingProofRequester {
    /// Creates a retrying proof requester with default retry settings.
    pub fn new(inner: Arc<dyn ProofRequesterProvider>) -> Self {
        Self { inner, retry: ProofRequesterRetryConfig::default() }
    }

    /// Creates a retrying proof requester with the provided retry settings.
    pub const fn with_retry_config(
        inner: Arc<dyn ProofRequesterProvider>,
        retry: ProofRequesterRetryConfig,
    ) -> Self {
        Self { inner, retry }
    }
}

impl std::fmt::Debug for RetryingProofRequester {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RetryingProofRequester").field("retry", &self.retry).finish_non_exhaustive()
    }
}

#[async_trait]
impl ProofRequesterProvider for RetryingProofRequester {
    async fn prove_block_range(
        &self,
        request: ProveBlockRangeRequest,
    ) -> Result<ProveBlockRangeResponse, ProverServiceClientError> {
        let request_for_attempt = request.clone();
        (|| {
            let request = request_for_attempt.clone();

            async move { self.inner.prove_block_range(request).await }
        })
        .retry(self.retry.to_backoff_builder())
        .when(ProverServiceClientError::is_retryable)
        .notify(|error, delay| {
            warn!(
                session_id = ?request.proof.session_id,
                backoff_ms = delay.as_millis(),
                error = %error,
                "prove block range failed; retrying"
            );
        })
        .await
    }

    async fn get_proof(
        &self,
        request: GetProofRequest,
    ) -> Result<GetProofResponse, ProverServiceClientError> {
        let request_for_attempt = request.clone();
        (|| {
            let request = request_for_attempt.clone();

            async move { self.inner.get_proof(request).await }
        })
        .retry(self.retry.to_backoff_builder())
        .when(ProverServiceClientError::is_retryable)
        .notify(|error, delay| {
            warn!(
                session_id = %request.session_id,
                backoff_ms = delay.as_millis(),
                error = %error,
                "get proof failed; retrying"
            );
        })
        .await
    }

    async fn list_proofs(
        &self,
        request: ListProofsRequest,
    ) -> Result<ListProofsResponse, ProverServiceClientError> {
        (|| async { self.inner.list_proofs(request).await })
            .retry(self.retry.to_backoff_builder())
            .when(ProverServiceClientError::is_retryable)
            .notify(|error, delay| {
                warn!(
                    offset = request.offset,
                    limit = request.limit,
                    status_filter = ?request.status_filter,
                    backoff_ms = delay.as_millis(),
                    error = %error,
                    "list proofs failed; retrying"
                );
            })
            .await
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, sync::Mutex, time::Duration};

    use base_prover_service_protocol::{ProofRequest, ProofRequestKind, ZkProofRequest, ZkVm};

    use super::*;

    enum MockOutcome<T> {
        Ok(T),
        Retryable,
        Fatal,
    }

    struct MockRequester {
        prove_outcomes: Mutex<VecDeque<MockOutcome<ProveBlockRangeResponse>>>,
        get_outcomes: Mutex<VecDeque<MockOutcome<GetProofResponse>>>,
        list_outcomes: Mutex<VecDeque<MockOutcome<ListProofsResponse>>>,
        prove_calls: Mutex<u32>,
        get_calls: Mutex<u32>,
        list_calls: Mutex<u32>,
    }

    impl MockRequester {
        fn new() -> Self {
            Self {
                prove_outcomes: Mutex::new(VecDeque::new()),
                get_outcomes: Mutex::new(VecDeque::new()),
                list_outcomes: Mutex::new(VecDeque::new()),
                prove_calls: Mutex::new(0),
                get_calls: Mutex::new(0),
                list_calls: Mutex::new(0),
            }
        }

        fn outcome<T>(outcome: MockOutcome<T>) -> Result<T, ProverServiceClientError> {
            match outcome {
                MockOutcome::Ok(value) => Ok(value),
                MockOutcome::Retryable => {
                    Err(ProverServiceClientError::Timeout("retryable".to_owned()))
                }
                MockOutcome::Fatal => {
                    Err(ProverServiceClientError::ProofFailure { message: "fatal".to_owned() })
                }
            }
        }
    }

    #[async_trait]
    impl ProofRequesterProvider for MockRequester {
        async fn prove_block_range(
            &self,
            _request: ProveBlockRangeRequest,
        ) -> Result<ProveBlockRangeResponse, ProverServiceClientError> {
            *self.prove_calls.lock().unwrap() += 1;
            let outcome = self.prove_outcomes.lock().unwrap().pop_front().expect("missing outcome");
            Self::outcome(outcome)
        }

        async fn get_proof(
            &self,
            _request: GetProofRequest,
        ) -> Result<GetProofResponse, ProverServiceClientError> {
            *self.get_calls.lock().unwrap() += 1;
            let outcome = self.get_outcomes.lock().unwrap().pop_front().expect("missing outcome");
            Self::outcome(outcome)
        }

        async fn list_proofs(
            &self,
            _request: ListProofsRequest,
        ) -> Result<ListProofsResponse, ProverServiceClientError> {
            *self.list_calls.lock().unwrap() += 1;
            let outcome = self.list_outcomes.lock().unwrap().pop_front().expect("missing outcome");
            Self::outcome(outcome)
        }
    }

    fn retry_config() -> ProofRequesterRetryConfig {
        ProofRequesterRetryConfig::new(3, Duration::from_millis(1), Duration::from_millis(1))
    }

    fn prove_request() -> ProveBlockRangeRequest {
        ProveBlockRangeRequest {
            proof: ProofRequest {
                session_id: Some("session".to_owned()),
                request: ProofRequestKind::Compressed(ZkProofRequest {
                    start_block_number: 1,
                    number_of_blocks_to_prove: 1,
                    sequence_window: None,
                    l1_head: None,
                    intermediate_root_interval: None,
                    zk_vm: ZkVm::Sp1,
                }),
            },
        }
    }

    #[tokio::test]
    async fn retrying_requester_retries_retryable_prove_block_range() {
        let inner = Arc::new(MockRequester::new());
        inner.prove_outcomes.lock().unwrap().extend([
            MockOutcome::Retryable,
            MockOutcome::Ok(ProveBlockRangeResponse { session_id: "session".to_owned() }),
        ]);
        let requester = RetryingProofRequester::with_retry_config(
            Arc::clone(&inner) as Arc<dyn ProofRequesterProvider>,
            retry_config(),
        );

        let response = requester.prove_block_range(prove_request()).await.unwrap();

        assert_eq!(response.session_id, "session");
        assert_eq!(*inner.prove_calls.lock().unwrap(), 2);
    }

    #[tokio::test]
    async fn retrying_requester_propagates_final_error_when_retries_exhausted() {
        let config = retry_config();
        let inner = Arc::new(MockRequester::new());
        // backon's `with_max_times(n)` allows `n` retries on top of the initial call,
        // so an exhausted run performs `max_attempts + 1` total calls.
        let total_calls = config.normalized_max_attempts() + 1;
        inner
            .prove_outcomes
            .lock()
            .unwrap()
            .extend((0..total_calls).map(|_| MockOutcome::Retryable));
        let requester = RetryingProofRequester::with_retry_config(
            Arc::clone(&inner) as Arc<dyn ProofRequesterProvider>,
            config,
        );

        let err = requester.prove_block_range(prove_request()).await.unwrap_err();

        assert!(matches!(err, ProverServiceClientError::Timeout(_)));
        assert!(err.is_retryable());
        assert_eq!(*inner.prove_calls.lock().unwrap(), total_calls);
    }

    #[tokio::test]
    async fn retrying_requester_does_not_retry_fatal_get_proof() {
        let inner = Arc::new(MockRequester::new());
        inner.get_outcomes.lock().unwrap().push_back(MockOutcome::Fatal);
        let requester = RetryingProofRequester::with_retry_config(
            Arc::clone(&inner) as Arc<dyn ProofRequesterProvider>,
            retry_config(),
        );

        let err = requester
            .get_proof(GetProofRequest { session_id: "session".to_owned() })
            .await
            .unwrap_err();

        assert!(!err.is_retryable());
        assert_eq!(*inner.get_calls.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn retrying_requester_retries_list_proofs() {
        let inner = Arc::new(MockRequester::new());
        inner.list_outcomes.lock().unwrap().extend([
            MockOutcome::Retryable,
            MockOutcome::Ok(ListProofsResponse { proofs: Vec::new(), total_count: 0 }),
        ]);
        let requester = RetryingProofRequester::with_retry_config(
            Arc::clone(&inner) as Arc<dyn ProofRequesterProvider>,
            retry_config(),
        );

        let response = requester
            .list_proofs(ListProofsRequest { offset: 0, limit: 10, status_filter: None })
            .await
            .unwrap();

        assert_eq!(response.total_count, 0);
        assert_eq!(*inner.list_calls.lock().unwrap(), 2);
    }
}
