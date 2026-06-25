//! Validates a completed proof against fresh canonical roots and submits it to L1.

use std::{sync::Arc, time::Duration};

use alloy_primitives::{Address, B256, Bytes};
use base_proof_contracts::{AggregateVerifierClient, DisputeGameFactoryClient, encode_extra_data};
use base_proof_primitives::Proposal;
use base_proof_rpc::RollupProvider;
use base_proof_submission::ProofSubmissionError;
use futures::{StreamExt, stream};
use tracing::{debug, info, instrument, warn};

use crate::{
    Metrics, RecoveredState, driver::DriverConfig, error::ProposerError,
    output_proposer::OutputProposer, proposal_intervals::ProposalIntervals,
};

const RECENT_GAME_LOOKUP_MAX_ATTEMPTS: usize = 3;
const RECENT_GAME_LOOKUP_RETRY_DELAY: Duration = Duration::from_millis(100);

/// Internal action returned to the pipeline after a single submission attempt.
///
/// The pipeline uses this to decide whether to retry, drop the cached
/// recovery, re-prove the target, or chain into the next submission.
#[derive(Debug)]
pub enum SubmitAction {
    /// Output root mismatch — the proved root no longer matches the canonical
    /// chain. The pipeline drops the cached recovery and re-proves on the
    /// next tick.
    RootMismatch,
    /// The dispute game already exists onchain by a previous attempt whose
    /// result was lost to an RPC propagation delay. The pipeline must
    /// invalidate its recovery cache so the next forward walk discovers the
    /// existing game.
    GameAlreadyExists,
    /// Transient failure — retry later with the same proof.
    Failed(ProposerError),
    /// Proof is permanently invalid (e.g. signer not registered) — discard
    /// and re-prove on the next attempt.
    Discard(ProposerError),
}

/// Validates a TEE proof against the canonical chain and submits it to L1.
#[derive(Clone)]
pub struct ProofSubmitter {
    output_proposer: Arc<dyn OutputProposer>,
    rollup_client: Arc<dyn RollupProvider>,
    factory_client: Arc<dyn DisputeGameFactoryClient>,
    verifier_client: Arc<dyn AggregateVerifierClient>,
    game_type: u32,
    block_interval: u64,
    intermediate_block_interval: u64,
    output_fetch_concurrency: usize,
}

impl std::fmt::Debug for ProofSubmitter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProofSubmitter")
            .field("game_type", &self.game_type)
            .field("block_interval", &self.block_interval)
            .field("intermediate_block_interval", &self.intermediate_block_interval)
            .field("output_fetch_concurrency", &self.output_fetch_concurrency)
            .finish_non_exhaustive()
    }
}

impl ProofSubmitter {
    /// Creates a new proof submitter.
    pub fn new(
        output_proposer: Arc<dyn OutputProposer>,
        rollup_client: Arc<dyn RollupProvider>,
        factory_client: Arc<dyn DisputeGameFactoryClient>,
        verifier_client: Arc<dyn AggregateVerifierClient>,
        config: &DriverConfig,
    ) -> Self {
        Self {
            output_proposer,
            rollup_client,
            factory_client,
            verifier_client,
            game_type: config.game_type,
            block_interval: config.block_interval,
            intermediate_block_interval: config.intermediate_block_interval,
            output_fetch_concurrency: config.recovery_scan_concurrency,
        }
    }

    /// Validates the completed proof and submits it to L1 as a dispute game.
    ///
    /// Returns the next recovered state when the proof was submitted or
    /// attached and the game address is known. Any other outcome — including
    /// RPC failures, root mismatches, invalid signers, or contract-level
    /// rejections — is mapped to a [`SubmitAction`] variant that tells the
    /// pipeline how to react.
    #[instrument(skip_all, fields(target_block = target_block, parent_address = %parent_address))]
    pub async fn submit(
        &self,
        aggregate_proposal: &Proposal,
        proposals: &[Proposal],
        target_block: u64,
        parent_address: Address,
    ) -> Result<RecoveredState, SubmitAction> {
        // JIT validation: check that the proved output root still matches canonical.
        let canonical_output = self
            .rollup_client
            .fresh_output_at_block(target_block)
            .await
            .map_err(|e| SubmitAction::Failed(ProposerError::Rpc(e)))?;

        if aggregate_proposal.output_root != canonical_output.output_root {
            warn!(
                proposal_root = ?aggregate_proposal.output_root,
                canonical_root = ?canonical_output.output_root,
                target_block,
                "Proposal output root does not match canonical chain at submit time"
            );
            return Err(SubmitAction::RootMismatch);
        }

        // Extract intermediate roots.
        let starting_block_number =
            target_block.checked_sub(self.block_interval).ok_or_else(|| {
                SubmitAction::Failed(ProposerError::Internal(format!(
                    "target_block {target_block} < block_interval {}",
                    self.block_interval
                )))
            })?;
        let intermediate_blocks = ProposalIntervals::intermediate_block_numbers(
            self.block_interval,
            self.intermediate_block_interval,
            starting_block_number,
        )
        .map_err(SubmitAction::Failed)?;
        let intermediate_roots = Self::extract_intermediate_roots(
            starting_block_number,
            proposals,
            &intermediate_blocks,
        )
        .map_err(SubmitAction::Failed)?;

        let rollup_client = Arc::clone(&self.rollup_client);
        let target_root = canonical_output.output_root;
        let canonical_roots = stream::iter(intermediate_blocks.iter().copied())
            .map(|block| {
                let rollup_client = Arc::clone(&rollup_client);
                async move {
                    if block == target_block {
                        Ok(target_root)
                    } else {
                        rollup_client
                            .fresh_output_at_block(block)
                            .await
                            .map(|out| out.output_root)
                            .map_err(|error| SubmitAction::Failed(ProposerError::Rpc(error)))
                    }
                }
            })
            .buffered(self.output_fetch_concurrency.max(1))
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<Result<Vec<_>, _>>()?;

        for ((block, proposal_root), canonical_root) in intermediate_blocks
            .iter()
            .copied()
            .zip(intermediate_roots.iter().copied())
            .zip(canonical_roots)
        {
            if proposal_root != canonical_root {
                warn!(
                    intermediate_block = block,
                    proposal_root = ?proposal_root,
                    canonical_root = ?canonical_root,
                    target_block,
                    "Intermediate output root does not match canonical chain at submit time"
                );
                return Err(SubmitAction::RootMismatch);
            }
        }

        let extra_data = encode_extra_data(target_block, parent_address, &intermediate_roots);
        let existing_game = self
            .factory_client
            .games(self.game_type, aggregate_proposal.output_root, extra_data.clone())
            .await
            .map_err(|e| {
                SubmitAction::Failed(ProposerError::Contract(format!(
                    "matching game lookup failed: {e}"
                )))
            })?;

        if existing_game != Address::ZERO {
            return self
                .attach_existing_game_proof(existing_game, aggregate_proposal, target_block)
                .await;
        }

        info!(
            target_block,
            output_root = ?aggregate_proposal.output_root,
            parent_address = %parent_address,
            intermediate_roots_count = intermediate_roots.len(),
            proposals_count = proposals.len(),
            "Proposing output (creating dispute game)"
        );

        let mut propose_timer = base_metrics::timed!(Metrics::proposal_l1_tx_duration_seconds());
        let propose_result = self
            .output_proposer
            .propose_output(aggregate_proposal, parent_address, &intermediate_roots)
            .await;

        match propose_result {
            Ok(()) => {
                drop(propose_timer);
                info!(target_block, "Dispute game created successfully");
                Metrics::l2_output_proposals_total().increment(1);
                let game_address = self
                    .lookup_recent_game(aggregate_proposal.output_root, &extra_data, target_block)
                    .await?;
                Ok(RecoveredState {
                    parent_address: game_address,
                    output_root: aggregate_proposal.output_root,
                    l2_block_number: target_block,
                })
            }
            Err(ProposerError::Submission(ProofSubmissionError::GameAlreadyExists)) => {
                drop(propose_timer);
                info!(target_block, "Game already exists, checking fresh state from chain");
                let raced_game = self
                    .lookup_recent_game(aggregate_proposal.output_root, &extra_data, target_block)
                    .await?;
                self.attach_existing_game_proof(raced_game, aggregate_proposal, target_block).await
            }
            Err(e) => {
                propose_timer.disarm();
                Err(Self::submission_error_action(e, target_block, None))
            }
        }
    }

    async fn lookup_recent_game(
        &self,
        output_root: B256,
        extra_data: &Bytes,
        target_block: u64,
    ) -> Result<Address, SubmitAction> {
        for attempt in 1..=RECENT_GAME_LOOKUP_MAX_ATTEMPTS {
            match self.factory_client.games(self.game_type, output_root, extra_data.clone()).await {
                Ok(game_address) if game_address != Address::ZERO => {
                    if attempt > 1 {
                        info!(
                            target_block,
                            output_root = ?output_root,
                            game_address = %game_address,
                            attempt,
                            "Dispute game found by UUID lookup after retry"
                        );
                    }
                    return Ok(game_address);
                }
                Ok(_) => {
                    if attempt == RECENT_GAME_LOOKUP_MAX_ATTEMPTS {
                        info!(
                            target_block,
                            output_root = ?output_root,
                            "Dispute game not found by UUID lookup after retries"
                        );
                        return Err(SubmitAction::GameAlreadyExists);
                    }
                    debug!(
                        target_block,
                        output_root = ?output_root,
                        attempt,
                        max_attempts = RECENT_GAME_LOOKUP_MAX_ATTEMPTS,
                        "Dispute game not found by UUID lookup, retrying"
                    );
                }
                Err(error) => {
                    if attempt == RECENT_GAME_LOOKUP_MAX_ATTEMPTS {
                        return Err(SubmitAction::Failed(ProposerError::Contract(format!(
                            "matching game lookup failed: {error}"
                        ))));
                    }
                    warn!(
                        target_block,
                        output_root = ?output_root,
                        attempt,
                        max_attempts = RECENT_GAME_LOOKUP_MAX_ATTEMPTS,
                        error = %error,
                        "Dispute game UUID lookup failed, retrying"
                    );
                }
            }

            tokio::time::sleep(RECENT_GAME_LOOKUP_RETRY_DELAY).await;
        }

        Err(SubmitAction::GameAlreadyExists)
    }

    async fn attach_existing_game_proof(
        &self,
        game_address: Address,
        aggregate_proposal: &Proposal,
        target_block: u64,
    ) -> Result<RecoveredState, SubmitAction> {
        let game_l1_head = self.verifier_client.l1_head(game_address).await.map_err(|e| {
            SubmitAction::Failed(ProposerError::Contract(format!(
                "l1Head lookup failed for game {game_address}: {e}"
            )))
        })?;
        if game_l1_head != aggregate_proposal.l1_origin_hash {
            info!(
                target_block,
                game_address = %game_address,
                game_l1_head = ?game_l1_head,
                proof_l1_origin = ?aggregate_proposal.l1_origin_hash,
                "Existing dispute game uses a different L1 head, recovering chain state"
            );
            return Err(SubmitAction::GameAlreadyExists);
        }

        info!(
            target_block,
            game_address = %game_address,
            output_root = ?aggregate_proposal.output_root,
            "Attaching TEE proof to existing dispute game"
        );

        let mut attach_timer = base_metrics::timed!(Metrics::proposal_l1_tx_duration_seconds());
        match self.output_proposer.verify_proposal_proof(game_address, aggregate_proposal).await {
            Ok(()) => {
                drop(attach_timer);
                info!(target_block, game_address = %game_address, "TEE proof attached successfully");
                Metrics::l2_output_proposals_total().increment(1);
            }
            Err(ProposerError::Submission(ProofSubmissionError::ProofAlreadyVerified)) => {
                drop(attach_timer);
                info!(
                    target_block,
                    game_address = %game_address,
                    "TEE proof was attached by another submitter"
                );
            }
            Err(e) => {
                attach_timer.disarm();
                return Err(Self::submission_error_action(e, target_block, Some(game_address)));
            }
        };

        Ok(RecoveredState {
            parent_address: game_address,
            output_root: aggregate_proposal.output_root,
            l2_block_number: target_block,
        })
    }

    fn submission_error_action(
        error: ProposerError,
        target_block: u64,
        game_address: Option<Address>,
    ) -> SubmitAction {
        let discard = matches!(
            error,
            ProposerError::Submission(
                ProofSubmissionError::InvalidSigner
                    | ProofSubmissionError::L1OriginTooOld
                    | ProofSubmissionError::InvalidParentGame
            )
        );

        if discard {
            if matches!(error, ProposerError::Submission(ProofSubmissionError::InvalidSigner)) {
                Metrics::tee_signer_invalid_total().increment(1);
            }
            warn!(
                error = %error,
                target_block,
                game_address = game_address.map(tracing::field::display),
                "Proof cannot be submitted with current chain state, discarding proof to re-prove"
            );
            return SubmitAction::Discard(error);
        }

        SubmitAction::Failed(error)
    }

    /// Extracts intermediate output roots from per-block proposals.
    ///
    /// Samples at every `intermediate_block_interval` within the range.
    fn extract_intermediate_roots(
        starting_block_number: u64,
        proposals: &[Proposal],
        blocks: &[u64],
    ) -> Result<Vec<B256>, ProposerError> {
        blocks
            .iter()
            .map(|&target_block| {
                let idx = target_block.checked_sub(starting_block_number + 1).ok_or_else(|| {
                    ProposerError::Internal(format!(
                        "underflow computing proposal index for block {target_block}"
                    ))
                })?;
                proposals.get(idx as usize).map(|proposal| proposal.output_root).ok_or_else(|| {
                    ProposerError::Internal(format!(
                        "intermediate root at block {target_block} not found in proposals (index {idx}, len {})",
                        proposals.len()
                    ))
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    #[cfg(feature = "metrics")]
    use metrics_util::{
        MetricKind,
        debugging::{DebugValue, DebuggingRecorder},
    };

    use super::*;
    use crate::test_utils::{
        MockAggregateVerifier, MockDisputeGameFactory, MockOutputProposer, MockRollupClient,
        test_proposal, test_sync_status,
    };

    const TEST_GAME_TYPE: u32 = 42;
    const TEST_BLOCK_INTERVAL: u64 = 100;

    fn proof_result(target_block: u64) -> (Proposal, Vec<Proposal>) {
        let mut aggregate_proposal = test_proposal(target_block);
        aggregate_proposal.l1_origin_hash = B256::ZERO;
        let proposals: Vec<Proposal> = (1..=target_block).map(test_proposal).collect();
        (aggregate_proposal, proposals)
    }

    fn submitter(
        output_proposer: Arc<MockOutputProposer>,
        factory: impl DisputeGameFactoryClient + 'static,
        verifier: MockAggregateVerifier,
    ) -> ProofSubmitter {
        ProofSubmitter::new(
            output_proposer,
            Arc::new(MockRollupClient {
                sync_status: test_sync_status(TEST_BLOCK_INTERVAL, B256::ZERO),
                output_roots: Default::default(),
                max_safe_block: None,
            }),
            Arc::new(factory),
            Arc::new(verifier),
            &DriverConfig {
                game_type: TEST_GAME_TYPE,
                block_interval: TEST_BLOCK_INTERVAL,
                intermediate_block_interval: TEST_BLOCK_INTERVAL,
                ..Default::default()
            },
        )
    }

    #[tokio::test]
    async fn submit_attaches_proof_to_existing_matching_game() {
        let game_address = Address::repeat_byte(0xAA);
        let output = Arc::new(MockOutputProposer::default());
        let submitter = submitter(
            Arc::clone(&output),
            MockDisputeGameFactory::with_uuid_game_responses([game_address]),
            MockAggregateVerifier::default(),
        );

        let (aggregate_proposal, proposals) = proof_result(TEST_BLOCK_INTERVAL);
        let result = submitter
            .submit(&aggregate_proposal, &proposals, TEST_BLOCK_INTERVAL, Address::ZERO)
            .await;

        assert!(result.is_ok());
        assert_eq!(*output.created.lock().unwrap(), 0);
        assert_eq!(*output.verified.lock().unwrap(), vec![game_address]);
    }

    #[cfg(feature = "metrics")]
    #[test]
    fn submit_does_not_count_already_verified_attach_as_submitted() {
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();

        ::metrics::with_local_recorder(&recorder, || {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            rt.block_on(async {
                let game_address = Address::repeat_byte(0xAA);
                let output = Arc::new(MockOutputProposer::default());
                *output.verify_error.lock().unwrap() =
                    Some(ProposerError::Submission(ProofSubmissionError::ProofAlreadyVerified));
                let submitter = submitter(
                    Arc::clone(&output),
                    MockDisputeGameFactory::with_uuid_game_responses([game_address]),
                    MockAggregateVerifier::default(),
                );

                let (aggregate_proposal, proposals) = proof_result(TEST_BLOCK_INTERVAL);
                submitter
                    .submit(&aggregate_proposal, &proposals, TEST_BLOCK_INTERVAL, Address::ZERO)
                    .await
                    .unwrap();
            });
        });

        let snapshot = snapshotter.snapshot().into_vec();
        assert!(snapshot.iter().all(|(ck, _, _, value)| {
            ck.kind() != MetricKind::Counter
                || ck.key().name() != "base_proposer.l2_output_proposals_total"
                || !matches!(value, DebugValue::Counter(value) if *value > 0)
        }));
    }

    #[tokio::test]
    async fn submit_validates_multiple_intermediate_roots() {
        let game_address = Address::repeat_byte(0xAA);
        let output = Arc::new(MockOutputProposer::default());
        let submitter = ProofSubmitter::new(
            output,
            Arc::new(MockRollupClient {
                sync_status: test_sync_status(TEST_BLOCK_INTERVAL, B256::ZERO),
                output_roots: Default::default(),
                max_safe_block: None,
            }),
            Arc::new(MockDisputeGameFactory::with_uuid_game_responses([game_address])),
            Arc::new(MockAggregateVerifier::default()),
            &DriverConfig {
                game_type: TEST_GAME_TYPE,
                block_interval: TEST_BLOCK_INTERVAL,
                intermediate_block_interval: 25,
                recovery_scan_concurrency: 2,
                ..Default::default()
            },
        );

        let (aggregate_proposal, proposals) = proof_result(TEST_BLOCK_INTERVAL);
        let result = submitter
            .submit(&aggregate_proposal, &proposals, TEST_BLOCK_INTERVAL, Address::ZERO)
            .await;

        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn submit_attaches_proof_after_game_already_exists_race() {
        let game_address = Address::repeat_byte(0xAA);
        let output = Arc::new(MockOutputProposer::with_create_error(ProposerError::Submission(
            ProofSubmissionError::GameAlreadyExists,
        )));
        let submitter = submitter(
            Arc::clone(&output),
            MockDisputeGameFactory::with_uuid_game_responses([Address::ZERO, game_address]),
            MockAggregateVerifier::default(),
        );

        let (aggregate_proposal, proposals) = proof_result(TEST_BLOCK_INTERVAL);
        let result = submitter
            .submit(&aggregate_proposal, &proposals, TEST_BLOCK_INTERVAL, Address::ZERO)
            .await;

        assert!(result.is_ok());
        assert_eq!(*output.created.lock().unwrap(), 1);
        assert_eq!(*output.verified.lock().unwrap(), vec![game_address]);
    }

    #[tokio::test(start_paused = true)]
    async fn submit_retries_created_game_lookup() {
        let game_address = Address::repeat_byte(0xAA);
        let output = Arc::new(MockOutputProposer::default());
        let submitter = submitter(
            Arc::clone(&output),
            MockDisputeGameFactory::with_uuid_game_responses([
                Address::ZERO,
                Address::ZERO,
                game_address,
            ]),
            MockAggregateVerifier::default(),
        );

        let (aggregate_proposal, proposals) = proof_result(TEST_BLOCK_INTERVAL);
        let result = submitter
            .submit(&aggregate_proposal, &proposals, TEST_BLOCK_INTERVAL, Address::ZERO)
            .await;

        let recovered = result.unwrap();
        assert_eq!(recovered.parent_address, game_address);
        assert_eq!(recovered.l2_block_number, TEST_BLOCK_INTERVAL);
        assert_eq!(*output.created.lock().unwrap(), 1);
        assert!(output.verified.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn submit_discards_invalid_parent_game_create_error() {
        let output = Arc::new(MockOutputProposer::with_create_error(ProposerError::Submission(
            ProofSubmissionError::InvalidParentGame,
        )));
        let submitter = submitter(
            Arc::clone(&output),
            MockDisputeGameFactory::with_games(vec![]),
            MockAggregateVerifier::default(),
        );

        let (aggregate_proposal, proposals) = proof_result(TEST_BLOCK_INTERVAL);
        let result = submitter
            .submit(&aggregate_proposal, &proposals, TEST_BLOCK_INTERVAL, Address::ZERO)
            .await;

        assert!(matches!(
            result,
            Err(SubmitAction::Discard(ref error))
                if error.metric_label() == "invalid_parent_game"
        ));
        assert_eq!(*output.created.lock().unwrap(), 1);
        assert!(output.verified.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn submit_recovers_when_existing_game_l1_head_mismatches() {
        let game_address = Address::repeat_byte(0xAA);
        let mut verifier = MockAggregateVerifier::default();
        verifier.l1_head_map.insert(game_address, B256::repeat_byte(0xCC));
        let output = Arc::new(MockOutputProposer::default());
        let submitter = submitter(
            Arc::clone(&output),
            MockDisputeGameFactory::with_uuid_game_responses([game_address]),
            verifier,
        );

        let (aggregate_proposal, proposals) = proof_result(TEST_BLOCK_INTERVAL);
        let result = submitter
            .submit(&aggregate_proposal, &proposals, TEST_BLOCK_INTERVAL, Address::ZERO)
            .await;

        assert!(matches!(result, Err(SubmitAction::GameAlreadyExists)));
        assert_eq!(*output.created.lock().unwrap(), 0);
        assert!(output.verified.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn submit_handles_existing_game_attach_error() {
        for (error, expected_discard_label) in [
            (ProposerError::Submission(ProofSubmissionError::ProofAlreadyVerified), None),
            (
                ProposerError::Submission(ProofSubmissionError::L1OriginTooOld),
                Some("l1_origin_too_old"),
            ),
            (
                ProposerError::Submission(ProofSubmissionError::InvalidParentGame),
                Some("invalid_parent_game"),
            ),
            (
                ProposerError::Submission(ProofSubmissionError::InvalidSigner),
                Some("invalid_signer"),
            ),
        ] {
            let game_address = Address::repeat_byte(0xAA);
            let output = Arc::new(MockOutputProposer::default());
            *output.verify_error.lock().unwrap() = Some(error);
            let submitter = submitter(
                Arc::clone(&output),
                MockDisputeGameFactory::with_uuid_game_responses([game_address]),
                MockAggregateVerifier::default(),
            );

            let (aggregate_proposal, proposals) = proof_result(TEST_BLOCK_INTERVAL);
            let result = submitter
                .submit(&aggregate_proposal, &proposals, TEST_BLOCK_INTERVAL, Address::ZERO)
                .await;

            match expected_discard_label {
                None => assert!(result.is_ok()),
                Some(label) => assert!(
                    matches!(result, Err(SubmitAction::Discard(ref error)) if error.metric_label() == label)
                ),
            }
            assert_eq!(*output.created.lock().unwrap(), 0);
            assert_eq!(*output.verified.lock().unwrap(), vec![game_address]);
        }
    }

    #[tokio::test]
    async fn submit_recovers_when_created_game_lookup_misses() {
        let output = Arc::new(MockOutputProposer::default());
        let submitter = submitter(
            Arc::clone(&output),
            MockDisputeGameFactory::with_games(vec![]),
            MockAggregateVerifier::default(),
        );

        let (aggregate_proposal, proposals) = proof_result(TEST_BLOCK_INTERVAL);
        let result = submitter
            .submit(&aggregate_proposal, &proposals, TEST_BLOCK_INTERVAL, Address::ZERO)
            .await;

        assert!(matches!(result, Err(SubmitAction::GameAlreadyExists)));
        assert_eq!(*output.created.lock().unwrap(), 1);
        assert!(output.verified.lock().unwrap().is_empty());
    }
}
