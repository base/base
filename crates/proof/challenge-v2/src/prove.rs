//! ZK and TEE proving for dispute violations.
//!
//! Entry point: [`Violation::dispute_request`], which produces a
//! [`DisputeRequest`] ready for the submission task. Decision policy:
//!
//! - For [`ViolationSituation::ZkWrong`], emit
//!   [`DisputeAction::NullifyZk`] asserting our computed root.
//! - For [`ViolationSituation::FraudulentZkChallenge`], emit
//!   [`DisputeAction::NullifyZk`] asserting the on-chain TEE root
//!   (contract-enforced equality).
//! - For [`ViolationSituation::TeeWrong`], try our TEE prover first;
//!   if it signs our computed root, emit
//!   [`DisputeAction::NullifyTee`]. Otherwise fall back to a ZK
//!   [`DisputeAction::Challenge`] so the game still resolves in our
//!   favor without flipping the global TEE killswitch.

use alloy_primitives::{B256, Bytes};
use async_trait::async_trait;
use base_proof_primitives::{PROOF_TYPE_ZK, ProofEncoder};
use base_zk_client::{
    GetProofRequest, ProofJobStatus, ProofType, ProveBlockRequest, ReceiptType, ZkProofError,
};
use thiserror::Error;
use tokio::time::Instant;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::{DisputeAction, DisputeRequest, Violation, ViolationSituation, WorkerDeps};

/// Output of a TEE proving call: the root the TEE attested to plus
/// the raw 65-byte ECDSA signature over it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeeProofResult {
    /// Output root the TEE attested to.
    pub signed_root: B256,
    /// Raw 65-byte ECDSA signature over `signed_root` (`r || s || v`).
    /// The encoder normalizes `v` to 27/28 before submission.
    pub signature_bytes: Bytes,
}

/// Errors returned by [`TeeProofProvider::prove_range`].
#[derive(Debug, Error)]
pub enum TeeProofError {
    /// TEE backend produced an error (transport, signing, attestation, ...).
    #[error("TEE backend error: {0}")]
    Backend(String),
}

/// TEE proving service abstraction.
///
/// Implementations sign an attestation over a range of L2 blocks
/// and return the signed root plus signature bytes. Cheap (seconds,
/// not hours) compared to ZK proving.
#[async_trait]
pub trait TeeProofProvider: Send + Sync + std::fmt::Debug {
    /// Asks the TEE to sign an attestation over the L2 block range
    /// `[start_block, end_block]` rooted at `l1_head`, sampling
    /// intermediate roots at the given checkpoint interval.
    async fn prove_range(
        &self,
        start_block: u64,
        end_block: u64,
        l1_head: B256,
        intermediate_block_interval: u64,
    ) -> Result<TeeProofResult, TeeProofError>;
}

/// Errors that can prevent [`Violation::dispute_request`] from emitting a [`DisputeRequest`].
#[derive(Debug, Error)]
pub enum ProofError {
    /// Underlying gRPC or transport error from the ZK service.
    #[error(transparent)]
    ZkClient(#[from] ZkProofError),
    /// ZK proof job reported `Failed` and no retry budget remained.
    #[error("ZK proof job failed: {error_message:?}")]
    ZkJobFailed {
        /// Server-reported error message, if any.
        error_message: Option<String>,
    },
    /// Last ZK polling attempt exceeded `max_proof_duration` and no
    /// retry budget remained.
    #[error("ZK proving exceeded max_proof_duration on every attempt")]
    ZkTimeout,
    /// All retry attempts exhausted without producing a successful
    /// proof and no specific terminal error to report.
    #[error("ZK proof retries exhausted")]
    RetriesExhausted,
}

impl Violation {
    /// Deterministic ZK session id derived from `(game_address, invalid_index)`.
    /// Stable across retries so the ZK service can deduplicate or
    /// requeue the existing job rather than starting from scratch.
    pub fn session_id(&self) -> String {
        let mut bytes = [0u8; 28];
        bytes[..20].copy_from_slice(self.game_address.as_slice());
        bytes[20..].copy_from_slice(&self.invalid_index.to_be_bytes());
        Uuid::new_v5(&Uuid::NAMESPACE_OID, &bytes).to_string()
    }

    /// Generates a proof for this violation and bundles it with the
    /// matching [`DisputeAction`] into a [`DisputeRequest`].
    ///
    /// See the module docs for the full decision policy.
    pub async fn dispute_request(self, deps: &WorkerDeps) -> Result<DisputeRequest, ProofError> {
        match self.situation {
            ViolationSituation::ZkWrong => self.nullify_zk_request(self.computed_root, deps).await,
            ViolationSituation::FraudulentZkChallenge { on_chain_tee_root } => {
                self.nullify_zk_request(on_chain_tee_root, deps).await
            }
            ViolationSituation::TeeWrong => {
                // A TEE nullify flips the global TEE killswitch and is
                // the right outcome when our TEE prover confirms the
                // divergence. Fall back to a ZK challenge if it cannot
                // (backend down or our TEE itself diverges).
                match self.try_nullify_tee_request(&*deps.tee_prover).await {
                    Some(req) => Ok(req),
                    None => self.challenge_request(deps).await,
                }
            }
        }
    }

    /// Generates a ZK proof and bundles it into a
    /// [`DisputeAction::NullifyZk`] request asserting `root_to_prove`
    /// at the violation's invalid index.
    async fn nullify_zk_request(
        &self,
        root_to_prove: B256,
        deps: &WorkerDeps,
    ) -> Result<DisputeRequest, ProofError> {
        let proof_bytes = self.obtain_zk_proof(deps).await?;

        Ok(DisputeRequest {
            game_address: self.game_address,
            action: DisputeAction::NullifyZk {
                index: self.invalid_index,
                root_to_prove,
                starting_root: self.starting_root,
                start_block: self.start_block,
                end_block: self.end_block,
            },
            proof_bytes,
        })
    }

    /// Generates a ZK proof and bundles it into a
    /// [`DisputeAction::Challenge`] request asserting the root we
    /// computed.
    async fn challenge_request(&self, deps: &WorkerDeps) -> Result<DisputeRequest, ProofError> {
        let proof_bytes = self.obtain_zk_proof(deps).await?;

        Ok(DisputeRequest {
            game_address: self.game_address,
            action: DisputeAction::Challenge {
                index: self.invalid_index,
                our_root: self.computed_root,
                starting_root: self.starting_root,
                start_block: self.start_block,
                end_block: self.end_block,
            },
            proof_bytes,
        })
    }

    /// Asks our TEE prover to attest the disputed range and packages
    /// the result into a [`DisputeAction::NullifyTee`] request.
    /// Returns `None` on any outcome that prevents us from building
    /// a valid attestation (backend error, divergent root, malformed
    /// signature) so the caller can switch to a ZK challenge.
    async fn try_nullify_tee_request(
        &self,
        tee_prover: &dyn TeeProofProvider,
    ) -> Option<DisputeRequest> {
        let attestation = match tee_prover
            .prove_range(
                self.start_block,
                self.end_block,
                self.l1_head,
                self.intermediate_block_interval,
            )
            .await
        {
            Ok(r) => r,
            Err(e) => {
                warn!(
                    game = %self.game_address,
                    error = %e,
                    "TEE prove_range failed, falling back to ZK Challenge"
                );
                return None;
            }
        };

        // Our TEE disagrees with what we computed from L2 RPC. The
        // contract requires the proven root to match the signer's
        // attestation, so we cannot proceed with a TEE-based dispute.
        // Fall back rather than flip the global TEE killswitch on the
        // basis of a single source we can no longer trust.
        if attestation.signed_root != self.computed_root {
            warn!(
                game = %self.game_address,
                signed_root = %attestation.signed_root,
                computed_root = %self.computed_root,
                "TEE signed a divergent root, falling back to ZK Challenge"
            );
            return None;
        }

        let proof_bytes =
            match ProofEncoder::encode_dispute_proof_bytes(&attestation.signature_bytes) {
                Ok(bytes) => bytes,
                Err(e) => {
                    warn!(
                        game = %self.game_address,
                        error = %e,
                        "TEE signature failed to encode, falling back to ZK Challenge"
                    );
                    return None;
                }
            };

        Some(DisputeRequest {
            game_address: self.game_address,
            action: DisputeAction::NullifyTee {
                index: self.invalid_index,
                our_root: self.computed_root,
                starting_root: self.starting_root,
                start_block: self.start_block,
                end_block: self.end_block,
            },
            proof_bytes,
        })
    }

    /// Submits a ZK proof job and polls until it succeeds or the
    /// retry budget is exhausted. Returns the SNARK receipt bytes
    /// prefixed with `PROOF_TYPE_ZK`, ready for `nullify`/`challenge`.
    ///
    /// The session id is derived from `(game_address, invalid_index)`
    /// and reused across retries so the ZK service can requeue the
    /// existing job instead of starting from scratch.
    async fn obtain_zk_proof(&self, deps: &WorkerDeps) -> Result<Bytes, ProofError> {
        let session_id = self.session_id();
        let request = ProveBlockRequest {
            start_block_number: self.start_block,
            number_of_blocks_to_prove: self.intermediate_block_interval,
            sequence_window: None,
            proof_type: ProofType::SnarkGroth16.into(),
            session_id: Some(session_id.clone()),
            // Hex-encode the addresses to match the gRPC string schema.
            prover_address: Some(format!("{:#x}", deps.config.sender_address)),
            l1_head: Some(format!("{:#x}", self.l1_head)),
            intermediate_root_interval: Some(self.intermediate_block_interval),
        };

        // Each attempt: submit (idempotent under the same session_id),
        // then poll until terminal status or per-attempt deadline.
        // Retry on either prove_block transport failure or poll-side
        // failure (job Failed, ZkTimeout, unexpected status).
        let mut last_err: Option<ProofError> = None;
        for attempt in 0..=deps.config.max_proof_retries {
            debug!(
                game = %self.game_address,
                attempt,
                session_id = %session_id,
                "submitting ZK proof job"
            );

            if let Err(e) = deps.zk_prover.prove_block(request.clone()).await {
                warn!(
                    game = %self.game_address,
                    attempt,
                    error = %e,
                    "ZK prove_block call failed"
                );
                last_err = Some(ProofError::ZkClient(e));
                continue;
            }

            match Self::poll_until_done(&session_id, deps).await {
                Ok(receipt) => {
                    // Prepend the proof type discriminator that
                    // `nullify` and `challenge` calldata expect.
                    let mut prefixed = Vec::with_capacity(1 + receipt.len());
                    prefixed.push(PROOF_TYPE_ZK);
                    prefixed.extend_from_slice(&receipt);
                    info!(
                        game = %self.game_address,
                        attempt,
                        receipt_len = receipt.len(),
                        "ZK proof succeeded"
                    );
                    return Ok(Bytes::from(prefixed));
                }
                Err(e) => {
                    warn!(
                        game = %self.game_address,
                        attempt,
                        error = %e,
                        "ZK proof attempt failed"
                    );
                    last_err = Some(e);
                }
            }
        }

        Err(last_err.unwrap_or(ProofError::RetriesExhausted))
    }

    /// Polls the ZK service for `session_id` until a terminal status
    /// is reached or the per-attempt deadline elapses. Successful
    /// runs return the raw receipt bytes (no proof type prefix yet).
    async fn poll_until_done(session_id: &str, deps: &WorkerDeps) -> Result<Vec<u8>, ProofError> {
        // Per-attempt deadline. Each retry of the outer loop gets a
        // fresh deadline by design: a retry after a service recovery
        // should have the full budget to complete.
        let deadline = Instant::now() + deps.config.max_proof_duration;

        loop {
            let response = deps
                .zk_prover
                .get_proof(GetProofRequest {
                    session_id: session_id.to_string(),
                    receipt_type: Some(ReceiptType::OnChainSnark as i32),
                })
                .await?;

            match ProofJobStatus::try_from(response.status) {
                Ok(ProofJobStatus::Succeeded) => return Ok(response.receipt),
                Ok(ProofJobStatus::Failed) => {
                    return Err(ProofError::ZkJobFailed { error_message: response.error_message });
                }
                Ok(ProofJobStatus::Created | ProofJobStatus::Pending | ProofJobStatus::Running) => {
                    // Treat the deadline as a hard ceiling: if the next
                    // poll would land past it, give up now rather than
                    // waste a sleep cycle on a job we'd time out anyway.
                    if Instant::now() + deps.config.proof_poll_interval >= deadline {
                        return Err(ProofError::ZkTimeout);
                    }
                    tokio::time::sleep(deps.config.proof_poll_interval).await;
                }
                // STATUS_UNSPECIFIED or any unknown numeric value: surface
                // as a job failure so the retry loop can decide whether
                // to try again.
                Ok(ProofJobStatus::Unspecified) | Err(_) => {
                    return Err(ProofError::ZkJobFailed {
                        error_message: Some(format!(
                            "unexpected proof job status: {}",
                            response.status
                        )),
                    });
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use alloy_primitives::{Address, B256, Bytes, address, b256};

    use super::*;
    use crate::{
        WorkerConfig,
        test_utils::{MockTeeProofProvider, MockZkProofProvider},
    };

    const GAME: Address = address!("00000000000000000000000000000000000000a1");
    const SENDER: Address = address!("00000000000000000000000000000000000000b2");
    const L1_HEAD: B256 = b256!("1111111111111111111111111111111111111111111111111111111111111111");
    const STARTING_ROOT: B256 =
        b256!("2222222222222222222222222222222222222222222222222222222222222222");
    const COMPUTED_ROOT: B256 =
        b256!("3333333333333333333333333333333333333333333333333333333333333333");
    const INTERVAL: u64 = 5;

    fn violation(situation: ViolationSituation) -> Violation {
        Violation {
            game_address: GAME,
            l1_head: L1_HEAD,
            intermediate_block_interval: INTERVAL,
            invalid_index: 2,
            computed_root: COMPUTED_ROOT,
            starting_root: STARTING_ROOT,
            start_block: 100,
            end_block: 100 + INTERVAL,
            situation,
        }
    }

    fn test_signature() -> Bytes {
        // 65-byte ECDSA-shaped signature; v=27 is already in the
        // accepted range so the encoder leaves it untouched.
        let mut sig = vec![0xAB; 65];
        sig[64] = 27;
        Bytes::from(sig)
    }

    fn config() -> WorkerConfig {
        WorkerConfig {
            sender_address: SENDER,
            max_proof_retries: 2,
            proof_poll_interval: Duration::from_millis(10),
            max_proof_duration: Duration::from_secs(30),
        }
    }

    fn deps(zk: Arc<MockZkProofProvider>, tee: Arc<MockTeeProofProvider>) -> WorkerDeps {
        WorkerDeps::new(zk, tee, config())
    }

    /// Convenience for tests that exercise only the ZK path. The TEE
    /// mock is wired in (mandatory in `WorkerDeps`) but never called.
    fn deps_zk_path(zk: Arc<MockZkProofProvider>) -> WorkerDeps {
        deps(zk, Arc::new(MockTeeProofProvider::new()))
    }

    mod session_id {
        use super::*;

        #[test]
        fn deterministic_for_same_inputs() {
            let v = violation(ViolationSituation::ZkWrong);
            assert_eq!(v.session_id(), v.session_id());
        }

        #[test]
        fn changes_when_index_changes() {
            let mut a = violation(ViolationSituation::ZkWrong);
            let mut b = violation(ViolationSituation::ZkWrong);
            a.invalid_index = 1;
            b.invalid_index = 2;
            assert_ne!(a.session_id(), b.session_id());
        }

        #[test]
        fn changes_when_address_changes() {
            let mut a = violation(ViolationSituation::ZkWrong);
            let mut b = violation(ViolationSituation::ZkWrong);
            a.game_address = address!("00000000000000000000000000000000000000aa");
            b.game_address = address!("00000000000000000000000000000000000000bb");
            assert_ne!(a.session_id(), b.session_id());
        }
    }

    mod obtain_zk_proof {
        use super::*;

        #[tokio::test]
        async fn success_on_first_attempt_returns_zk_prefixed_bytes() {
            let zk = Arc::new(MockZkProofProvider::new());
            zk.push_prove_ok();
            zk.push_get_succeeded(vec![0xDE, 0xAD, 0xBE, 0xEF]);

            let deps = deps_zk_path(Arc::clone(&zk));
            let v = violation(ViolationSituation::ZkWrong);

            let bytes = v.obtain_zk_proof(&deps).await.expect("must succeed");
            assert_eq!(bytes.as_ref(), &[PROOF_TYPE_ZK, 0xDE, 0xAD, 0xBE, 0xEF]);
            assert_eq!(zk.prove_calls().len(), 1);
            assert_eq!(zk.get_calls().len(), 1);
        }

        #[tokio::test]
        async fn retries_with_same_session_id_after_failed_status() {
            let zk = Arc::new(MockZkProofProvider::new());
            zk.push_prove_ok();
            zk.push_get_failed(Some("simulated".into()));
            zk.push_prove_ok();
            zk.push_get_succeeded(vec![0x01]);

            let deps = deps_zk_path(Arc::clone(&zk));
            let v = violation(ViolationSituation::ZkWrong);
            let expected_session = v.session_id();

            let bytes = v.obtain_zk_proof(&deps).await.expect("must succeed on second attempt");
            assert_eq!(bytes.as_ref(), &[PROOF_TYPE_ZK, 0x01]);

            let prove_calls = zk.prove_calls();
            assert_eq!(prove_calls.len(), 2);
            assert_eq!(prove_calls[0].session_id.as_deref(), Some(expected_session.as_str()));
            assert_eq!(prove_calls[1].session_id.as_deref(), Some(expected_session.as_str()));
        }

        #[tokio::test]
        async fn three_consecutive_failures_return_zk_job_failed() {
            let zk = Arc::new(MockZkProofProvider::new());
            for _ in 0..3 {
                zk.push_prove_ok();
                zk.push_get_failed(None);
            }

            let deps = deps_zk_path(Arc::clone(&zk));
            let v = violation(ViolationSituation::ZkWrong);

            let err = v.obtain_zk_proof(&deps).await.expect_err("must exhaust retries");
            assert!(matches!(err, ProofError::ZkJobFailed { .. }), "got {err:?}");
            assert_eq!(zk.prove_calls().len(), 3);
        }

        #[tokio::test]
        async fn timeout_triggers_retry_with_same_session_id() {
            let zk = Arc::new(MockZkProofProvider::new());
            // Attempt 0: a single Pending forces ZkTimeout because the
            // deadline check fires immediately (`now >= now`).
            zk.push_prove_ok();
            zk.push_get_pending();
            // Attempt 1: succeed so the retry loop terminates.
            zk.push_prove_ok();
            zk.push_get_succeeded(vec![0xFF]);

            let mut cfg = config();
            cfg.max_proof_duration = Duration::ZERO;
            let deps = WorkerDeps::new(
                Arc::<MockZkProofProvider>::clone(&zk),
                Arc::new(MockTeeProofProvider::new()),
                cfg,
            );

            let v = violation(ViolationSituation::ZkWrong);
            let expected_session = v.session_id();

            let bytes = v.obtain_zk_proof(&deps).await.expect("retry must succeed");
            assert_eq!(bytes.as_ref(), &[PROOF_TYPE_ZK, 0xFF]);

            let prove_calls = zk.prove_calls();
            assert_eq!(prove_calls.len(), 2);
            assert_eq!(prove_calls[0].session_id, prove_calls[1].session_id);
            assert_eq!(prove_calls[0].session_id.as_deref(), Some(expected_session.as_str()));
        }

        #[tokio::test]
        async fn timeout_on_every_attempt_returns_zk_timeout() {
            let zk = Arc::new(MockZkProofProvider::new());
            // 3 attempts, each forced to ZkTimeout via Duration::ZERO.
            for _ in 0..3 {
                zk.push_prove_ok();
                zk.push_get_pending();
            }

            let mut cfg = config();
            cfg.max_proof_duration = Duration::ZERO;
            let deps = WorkerDeps::new(
                Arc::<MockZkProofProvider>::clone(&zk),
                Arc::new(MockTeeProofProvider::new()),
                cfg,
            );

            let v = violation(ViolationSituation::ZkWrong);
            let err = v.obtain_zk_proof(&deps).await.expect_err("must exhaust retries");
            assert!(matches!(err, ProofError::ZkTimeout), "got {err:?}");
            assert_eq!(zk.prove_calls().len(), 3);
        }

        #[tokio::test]
        async fn fills_all_request_fields_from_violation_and_config() {
            let zk = Arc::new(MockZkProofProvider::new());
            zk.push_prove_ok();
            zk.push_get_succeeded(vec![]);

            let deps = deps_zk_path(Arc::clone(&zk));
            let v = violation(ViolationSituation::ZkWrong);
            let expected_session = v.session_id();
            let expected_l1 = format!("{L1_HEAD:#x}");
            let expected_sender = format!("{SENDER:#x}");

            v.obtain_zk_proof(&deps).await.expect("must succeed");

            let req = zk.prove_calls().pop().expect("one prove_block call");
            assert_eq!(req.start_block_number, 100);
            assert_eq!(req.number_of_blocks_to_prove, INTERVAL);
            assert_eq!(req.intermediate_root_interval, Some(INTERVAL));
            assert_eq!(req.l1_head.as_deref(), Some(expected_l1.as_str()));
            assert_eq!(req.prover_address.as_deref(), Some(expected_sender.as_str()));
            assert_eq!(req.session_id.as_deref(), Some(expected_session.as_str()));
            assert_eq!(req.proof_type, ProofType::SnarkGroth16 as i32);
        }
    }

    mod dispute_request {
        use super::*;

        #[tokio::test]
        async fn zk_wrong_emits_nullify_zk_with_computed_root() {
            let zk = Arc::new(MockZkProofProvider::new());
            zk.push_prove_ok();
            zk.push_get_succeeded(vec![0xCA, 0xFE]);

            let deps = deps_zk_path(Arc::clone(&zk));
            let req = violation(ViolationSituation::ZkWrong)
                .dispute_request(&deps)
                .await
                .expect("must succeed");

            assert_eq!(req.game_address, GAME);
            match req.action {
                DisputeAction::NullifyZk { index, root_to_prove, .. } => {
                    assert_eq!(index, 2);
                    assert_eq!(root_to_prove, COMPUTED_ROOT);
                }
                other => panic!("expected NullifyZk, got {other}"),
            }
            assert_eq!(req.proof_bytes.as_ref(), &[PROOF_TYPE_ZK, 0xCA, 0xFE]);
        }

        #[tokio::test]
        async fn fraudulent_zk_challenge_emits_nullify_zk_with_on_chain_tee_root() {
            let on_chain_tee =
                b256!("4444444444444444444444444444444444444444444444444444444444444444");
            let zk = Arc::new(MockZkProofProvider::new());
            zk.push_prove_ok();
            zk.push_get_succeeded(vec![]);

            let deps = deps_zk_path(Arc::clone(&zk));
            let req = violation(ViolationSituation::FraudulentZkChallenge {
                on_chain_tee_root: on_chain_tee,
            })
            .dispute_request(&deps)
            .await
            .expect("must succeed");

            match req.action {
                DisputeAction::NullifyZk { root_to_prove, .. } => {
                    assert_eq!(root_to_prove, on_chain_tee);
                }
                other => panic!("expected NullifyZk, got {other}"),
            }
        }

        #[tokio::test]
        async fn tee_wrong_with_matching_local_tee_emits_nullify_tee() {
            let tee = Arc::new(MockTeeProofProvider::new());
            tee.push_ok(COMPUTED_ROOT, test_signature());
            let zk = Arc::new(MockZkProofProvider::new());

            let deps = deps(Arc::clone(&zk), Arc::clone(&tee));
            let req = violation(ViolationSituation::TeeWrong)
                .dispute_request(&deps)
                .await
                .expect("must succeed");

            assert_eq!(req.game_address, GAME);
            match req.action {
                DisputeAction::NullifyTee { index, our_root, .. } => {
                    assert_eq!(index, 2);
                    assert_eq!(our_root, COMPUTED_ROOT);
                }
                other => panic!("expected NullifyTee, got {other}"),
            }
            // Encoded as `[PROOF_TYPE_TEE(0), signature(65)]`.
            assert_eq!(req.proof_bytes.len(), 66);
            assert_eq!(req.proof_bytes[0], 0);
            // ZK provider must not have been called on the happy TEE path.
            assert_eq!(zk.prove_calls().len(), 0);
        }

        #[tokio::test]
        async fn tee_wrong_with_diverging_local_tee_falls_back_to_challenge() {
            let bad_root =
                b256!("5555555555555555555555555555555555555555555555555555555555555555");
            let tee = Arc::new(MockTeeProofProvider::new());
            tee.push_ok(bad_root, test_signature());

            let zk = Arc::new(MockZkProofProvider::new());
            zk.push_prove_ok();
            zk.push_get_succeeded(vec![0x77]);

            let deps = deps(Arc::clone(&zk), Arc::clone(&tee));
            let req = violation(ViolationSituation::TeeWrong)
                .dispute_request(&deps)
                .await
                .expect("must fall back to Challenge");

            match req.action {
                DisputeAction::Challenge { our_root, .. } => assert_eq!(our_root, COMPUTED_ROOT),
                other => panic!("expected Challenge, got {other}"),
            }
            assert_eq!(req.proof_bytes.as_ref(), &[PROOF_TYPE_ZK, 0x77]);
            assert_eq!(zk.prove_calls().len(), 1);
        }

        #[tokio::test]
        async fn tee_wrong_with_tee_backend_error_falls_back_to_challenge() {
            let tee = Arc::new(MockTeeProofProvider::new());
            tee.push_err(TeeProofError::Backend("simulated".into()));

            let zk = Arc::new(MockZkProofProvider::new());
            zk.push_prove_ok();
            zk.push_get_succeeded(vec![]);

            let deps = deps(Arc::clone(&zk), Arc::clone(&tee));
            let req = violation(ViolationSituation::TeeWrong)
                .dispute_request(&deps)
                .await
                .expect("must fall back to Challenge");

            assert!(matches!(req.action, DisputeAction::Challenge { .. }));
        }
    }
}
