//! Validates a completed proof and submits it to L1 as a new dispute game.
//!
//! The [`ProofSubmitter`] is invoked by the pipeline coordinator on a
//! per-target basis after a proof is collected and the pipeline determines it
//! is the next sequential proposal. The submitter performs:
//!
//! 1. JIT validation: re-fetch the canonical output root from the rollup node
//!    and verify it still matches the proved aggregate proposal root.
//! 2. Intermediate-root validation: extract the per-block intermediate roots
//!    from the underlying proposals, fetch fresh canonical roots for each
//!    intermediate block, and verify they all match the canonical chain.
//! 3. Optional pre-submission TEE signer validation against the onchain
//!    `TEEProverRegistry`.
//! 4. Calls `output_proposer.propose_output(..)` to create the dispute game on
//!    L1 and maps contract-level errors into the [`SubmitAction`] variants the
//!    pipeline interprets.
//!
//! Each [`ProofSubmitter::submit`] call is independent and reads no pipeline
//! state. The pipeline enforces single-flight submission via its own join set,
//! while the submitter focuses on per-proposal validation and the L1 call.

use std::{collections::HashMap, sync::Arc};

use alloy_eips::BlockNumberOrTag;
use alloy_primitives::{Address, B256, Signature, keccak256};
use alloy_sol_types::SolCall;
use base_proof_contracts::{
    AggregateVerifierClient, DisputeGameFactoryClient, ITEEProverRegistry, encode_extra_data,
};
use base_proof_primitives::{ProofJournal, ProofResult, Proposal};
use base_proof_rpc::{L1Provider, RollupProvider};
use base_proof_submission::ProofSubmissionError;
use futures::{StreamExt, stream};
use tracing::{debug, info, instrument, warn};

use crate::{
    Metrics, error::ProposerError, output_proposer::OutputProposer,
    proposal_intervals::ProposalIntervals,
};

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

impl std::fmt::Display for SubmitAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RootMismatch => write!(f, "output root mismatch"),
            Self::GameAlreadyExists => write!(f, "game already exists"),
            Self::Failed(e) | Self::Discard(e) => write!(f, "{e}"),
        }
    }
}

/// Configuration for [`ProofSubmitter`].
///
/// Bundles the scalar parameters the submitter needs at construction time so
/// [`ProofSubmitter::new`] takes a small, fixed argument list. All fields are
/// derived from the parent [`crate::PipelineConfig`].
#[derive(Debug, Clone, Copy)]
pub struct ProofSubmitterConfig {
    /// Address of the proposer on L1.
    pub proposer_address: Address,
    /// Dispute game type used for proposals.
    pub game_type: u32,
    /// Number of L2 blocks per proposal.
    pub block_interval: u64,
    /// Stride (in L2 blocks) between intermediate roots within a proposal.
    pub intermediate_block_interval: u64,
    /// Expected TEE enclave image hash.
    pub tee_image_hash: B256,
    /// Optional onchain `TEEProverRegistry` address. When set, the submitter
    /// performs an `isValidSigner` pre-flight check before calling
    /// `propose_output`.
    pub tee_prover_registry_address: Option<Address>,
    /// Concurrency limit for fetching canonical output roots from the rollup
    /// node during validation.
    pub output_fetch_concurrency: usize,
}

/// Validates a TEE proof against the canonical chain and submits it to L1.
pub struct ProofSubmitter<L1, R>
where
    L1: L1Provider,
    R: RollupProvider,
{
    output_proposer: Arc<dyn OutputProposer>,
    rollup_client: Arc<R>,
    l1_client: Arc<L1>,
    factory_client: Arc<dyn DisputeGameFactoryClient>,
    verifier_client: Arc<dyn AggregateVerifierClient>,
    config: ProofSubmitterConfig,
}

impl<L1, R> Clone for ProofSubmitter<L1, R>
where
    L1: L1Provider,
    R: RollupProvider,
{
    fn clone(&self) -> Self {
        Self {
            output_proposer: Arc::clone(&self.output_proposer),
            rollup_client: Arc::clone(&self.rollup_client),
            l1_client: Arc::clone(&self.l1_client),
            factory_client: Arc::clone(&self.factory_client),
            verifier_client: Arc::clone(&self.verifier_client),
            config: self.config,
        }
    }
}

impl<L1, R> std::fmt::Debug for ProofSubmitter<L1, R>
where
    L1: L1Provider,
    R: RollupProvider,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProofSubmitter").field("config", &self.config).finish_non_exhaustive()
    }
}

impl<L1, R> ProofSubmitter<L1, R>
where
    L1: L1Provider + 'static,
    R: RollupProvider + 'static,
{
    /// Creates a new proof submitter.
    pub const fn new(
        output_proposer: Arc<dyn OutputProposer>,
        rollup_client: Arc<R>,
        l1_client: Arc<L1>,
        factory_client: Arc<dyn DisputeGameFactoryClient>,
        verifier_client: Arc<dyn AggregateVerifierClient>,
        config: ProofSubmitterConfig,
    ) -> Self {
        Self { output_proposer, rollup_client, l1_client, factory_client, verifier_client, config }
    }

    /// Validates the completed proof and submits it to L1 as a dispute game.
    ///
    /// Returns `Ok(())` only when `propose_output` succeeded on L1. Any other
    /// outcome — including RPC failures, root mismatches, invalid signers, or
    /// contract-level rejections — is mapped to a [`SubmitAction`] variant
    /// that tells the pipeline how to react.
    #[instrument(skip_all, fields(target_block = target_block, parent_address = %parent_address))]
    pub async fn submit(
        &self,
        proof_result: &ProofResult,
        target_block: u64,
        parent_address: Address,
    ) -> Result<(), SubmitAction> {
        let (aggregate_proposal, proposals) = match proof_result {
            ProofResult::Tee { aggregate_proposal, proposals } => (aggregate_proposal, proposals),
            ProofResult::Zk { .. } => {
                return Err(SubmitAction::Failed(ProposerError::Prover(
                    "unexpected ZK proof result from TEE prover".into(),
                )));
            }
        };

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
            target_block.checked_sub(self.config.block_interval).ok_or_else(|| {
                SubmitAction::Failed(ProposerError::Internal(format!(
                    "target_block {target_block} < block_interval {}",
                    self.config.block_interval
                )))
            })?;
        let intermediate_blocks =
            self.intermediate_block_numbers(starting_block_number).map_err(SubmitAction::Failed)?;
        let intermediate_roots = self
            .extract_intermediate_roots(starting_block_number, proposals, &intermediate_blocks)
            .map_err(SubmitAction::Failed)?;

        // Fetch fresh canonical roots for non-target intermediate blocks only;
        // the target block was already fetched fresh for the JIT check above.
        let non_target_blocks: Vec<u64> =
            intermediate_blocks.iter().copied().filter(|&b| b != target_block).collect();

        let mut canonical_map: HashMap<u64, B256> = self
            .fetch_fresh_canonical_roots(&non_target_blocks)
            .await
            .map_err(SubmitAction::Failed)?;
        canonical_map.insert(target_block, canonical_output.output_root);

        for (root, block) in intermediate_roots.iter().zip(intermediate_blocks.iter()) {
            let canonical = canonical_map.get(block).ok_or_else(|| {
                SubmitAction::Failed(ProposerError::Internal(format!(
                    "missing canonical root for intermediate block {block}"
                )))
            })?;
            if *root != *canonical {
                warn!(
                    intermediate_block = *block,
                    proposal_root = ?root,
                    canonical_root = ?canonical,
                    target_block,
                    "Intermediate output root does not match canonical chain at submit time"
                );
                return Err(SubmitAction::RootMismatch);
            }
        }

        // Pre-submission signer validation: if a TEE prover registry is
        // configured, recover the signer from the aggregate proposal signature
        // and check `isValidSigner` onchain. If the signer is invalid, skip
        // submission to avoid wasting gas on a transaction that will revert.
        if let Some(registry_address) = self.config.tee_prover_registry_address {
            match self
                .check_signer_validity(
                    aggregate_proposal,
                    starting_block_number,
                    &intermediate_roots,
                    registry_address,
                )
                .await
            {
                Ok(true) => {}
                Ok(false) => {
                    // The proof's signer is not registered onchain. Discard
                    // this proof so the pipeline re-proves with a (potentially
                    // different, registered) enclave on the next attempt.
                    warn!(target_block, "TEE signer is not valid onchain, discarding proof");
                    Metrics::tee_signer_invalid_total().increment(1);
                    return Err(SubmitAction::Discard(ProposerError::Internal(
                        "TEE signer not registered onchain".into(),
                    )));
                }
                Err(e) => {
                    // Proceed on RPC failure: if L1 is unreachable, the
                    // subsequent propose_output call will also fail and be
                    // retried naturally. Blocking here would not save gas.
                    // This also handles the case where the registry contract
                    // is not yet deployed (rolling out the
                    // --tee-prover-registry-address config before the contract
                    // exists onchain).
                    warn!(error = %e, target_block, "signer validity check failed, proceeding anyway");
                }
            }
        }

        let extra_data = encode_extra_data(target_block, parent_address, &intermediate_roots);
        let existing_game = self
            .factory_client
            .games(self.config.game_type, aggregate_proposal.output_root, extra_data.clone())
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
                Ok(())
            }
            Err(e) => {
                if matches!(e, ProposerError::Submission(ProofSubmissionError::GameAlreadyExists)) {
                    drop(propose_timer);
                    info!(target_block, "Game already exists, checking fresh state from chain");
                    let raced_game = self
                        .factory_client
                        .games(self.config.game_type, aggregate_proposal.output_root, extra_data)
                        .await
                        .map_err(|e| {
                            SubmitAction::Failed(ProposerError::Contract(format!(
                                "matching game lookup after duplicate create failed: {e}"
                            )))
                        })?;
                    if raced_game != Address::ZERO {
                        return self
                            .attach_existing_game_proof(
                                raced_game,
                                aggregate_proposal,
                                target_block,
                            )
                            .await;
                    }

                    info!(
                        target_block,
                        "Game already exists, next tick will load fresh state from chain"
                    );
                    Err(SubmitAction::GameAlreadyExists)
                } else if matches!(
                    e,
                    ProposerError::Submission(ProofSubmissionError::L1OriginTooOld)
                ) {
                    propose_timer.disarm();
                    warn!(
                        error = %e,
                        target_block,
                        "Proof L1 origin is too old, discarding proof to re-prove"
                    );
                    Err(SubmitAction::Discard(e))
                } else if matches!(
                    e,
                    ProposerError::Submission(ProofSubmissionError::InvalidSigner)
                ) {
                    propose_timer.disarm();
                    warn!(
                        error = %e,
                        target_block,
                        "Proof signer is invalid onchain, discarding proof to re-prove"
                    );
                    Metrics::tee_signer_invalid_total().increment(1);
                    Err(SubmitAction::Discard(e))
                } else {
                    propose_timer.disarm();
                    Err(SubmitAction::Failed(e))
                }
            }
        }
    }

    async fn attach_existing_game_proof(
        &self,
        game_address: Address,
        aggregate_proposal: &Proposal,
        target_block: u64,
    ) -> Result<(), SubmitAction> {
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
                Ok(())
            }
            Err(ProposerError::Submission(ProofSubmissionError::ProofAlreadyVerified)) => {
                drop(attach_timer);
                info!(
                    target_block,
                    game_address = %game_address,
                    "TEE proof was attached by another submitter"
                );
                Ok(())
            }
            Err(e @ ProposerError::Submission(ProofSubmissionError::L1OriginTooOld)) => {
                attach_timer.disarm();
                warn!(
                    error = %e,
                    target_block,
                    game_address = %game_address,
                    "Proof L1 origin is too old, discarding proof to re-prove"
                );
                Err(SubmitAction::Discard(e))
            }
            Err(e @ ProposerError::Submission(ProofSubmissionError::InvalidSigner)) => {
                attach_timer.disarm();
                warn!(
                    error = %e,
                    target_block,
                    game_address = %game_address,
                    "Proof signer is invalid onchain, discarding proof to re-prove"
                );
                Metrics::tee_signer_invalid_total().increment(1);
                Err(SubmitAction::Discard(e))
            }
            Err(e) => {
                attach_timer.disarm();
                Err(SubmitAction::Failed(e))
            }
        }
    }

    /// Recovers the TEE signer from the aggregate proposal and checks
    /// `isValidSigner` on the `TEEProverRegistry`.
    ///
    /// Returns `Ok(true)` if the signer is valid, `Ok(false)` if not,
    /// or `Err` if the check itself failed (RPC error, parse failure, etc.).
    async fn check_signer_validity(
        &self,
        aggregate_proposal: &Proposal,
        starting_block_number: u64,
        intermediate_roots: &[B256],
        registry_address: Address,
    ) -> Result<bool, ProposerError> {
        // Reconstruct the journal that the enclave signed over.
        let journal = ProofJournal {
            proposer: self.config.proposer_address,
            l1_origin_hash: aggregate_proposal.l1_origin_hash,
            prev_output_root: aggregate_proposal.prev_output_root,
            starting_l2_block: starting_block_number,
            output_root: aggregate_proposal.output_root,
            ending_l2_block: aggregate_proposal.l2_block_number,
            intermediate_roots: intermediate_roots.to_vec(),
            config_hash: aggregate_proposal.config_hash,
            tee_image_hash: self.config.tee_image_hash,
        };
        let digest = keccak256(journal.encode());

        // Parse the 65-byte ECDSA signature (r ‖ s ‖ v).
        let sig_bytes = aggregate_proposal.signature.as_ref();
        let sig = Signature::try_from(sig_bytes)
            .map_err(|e| ProposerError::Internal(format!("invalid proposal signature: {e}")))?;

        let signer = sig
            .recover_address_from_prehash(&digest)
            .map_err(|e| ProposerError::Internal(format!("signer recovery failed: {e}")))?;

        debug!(signer = %signer, "recovered TEE signer from aggregate proposal");

        // Call isValidSigner on the registry via the L1 provider.
        let calldata = ITEEProverRegistry::isValidSignerCall { signer }.abi_encode();
        let result = self
            .l1_client
            .call_contract(registry_address, calldata.into(), BlockNumberOrTag::Latest)
            .await
            .map_err(ProposerError::Rpc)?;

        let is_valid =
            ITEEProverRegistry::isValidSignerCall::abi_decode_returns(&result).map_err(|e| {
                ProposerError::Internal(format!("failed to decode isValidSigner response: {e}"))
            })?;
        debug!(signer = %signer, is_valid, "isValidSigner check result");

        Ok(is_valid)
    }

    /// Returns intermediate block numbers between `starting_block_number` and
    /// the next proposal target, stepping by `intermediate_block_interval`.
    ///
    /// Used by submit validation to match the same checkpoint layout as
    /// proposer recovery.
    pub fn intermediate_block_numbers(
        &self,
        starting_block_number: u64,
    ) -> Result<Vec<u64>, ProposerError> {
        ProposalIntervals::intermediate_block_numbers(
            self.config.block_interval,
            self.config.intermediate_block_interval,
            starting_block_number,
        )
    }

    /// Extracts intermediate output roots from per-block proposals.
    ///
    /// Samples at every `intermediate_block_interval` within the range.
    fn extract_intermediate_roots(
        &self,
        starting_block_number: u64,
        proposals: &[Proposal],
        blocks: &[u64],
    ) -> Result<Vec<B256>, ProposerError> {
        let mut roots = Vec::with_capacity(blocks.len());
        for &target_block in blocks {
            let idx = target_block.checked_sub(starting_block_number + 1).ok_or_else(|| {
                ProposerError::Internal(format!(
                    "underflow computing proposal index for block {target_block}"
                ))
            })?;
            if let Some(p) = proposals.get(idx as usize) {
                roots.push(p.output_root);
            } else {
                return Err(ProposerError::Internal(format!(
                    "intermediate root at block {target_block} not found in proposals (index {idx}, len {})",
                    proposals.len()
                )));
            }
        }
        Ok(roots)
    }

    /// Concurrently fetches canonical output roots, bypassing the rollup
    /// client's output cache so each call hits the underlying node.
    async fn fetch_fresh_canonical_roots(
        &self,
        blocks: &[u64],
    ) -> Result<HashMap<u64, B256>, ProposerError> {
        if blocks.is_empty() {
            return Ok(HashMap::new());
        }
        let rollup = &self.rollup_client;
        stream::iter(blocks.iter().copied())
            .map(|block_number| async move {
                let result = rollup
                    .fresh_output_at_block(block_number)
                    .await
                    .map(|out| out.output_root)
                    .map_err(ProposerError::Rpc);
                (block_number, result)
            })
            .buffered(self.config.output_fetch_concurrency)
            .collect::<HashMap<_, _>>()
            .await
            .into_iter()
            .map(|(block_number, result)| result.map(|root| (block_number, root)))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{HashMap, VecDeque},
        sync::Arc,
    };

    use alloy_primitives::{Bytes, U256};
    use async_trait::async_trait;
    use base_proof_contracts::{ContractError, GameAtIndex, encode_extra_data};
    use base_proof_primitives::ProofResult;
    use rstest::rstest;

    use super::*;
    use crate::test_utils::{
        MockAggregateVerifier, MockDisputeGameFactory, MockL1, MockRollupClient, test_proposal,
        test_sync_status,
    };

    const TEST_GAME_TYPE: u32 = 42;
    const TEST_BLOCK_INTERVAL: u64 = 100;

    #[derive(Debug, Default)]
    struct RecordingOutputProposer {
        created: std::sync::Mutex<u32>,
        verified: std::sync::Mutex<Vec<Address>>,
        create_error: std::sync::Mutex<Option<ProposerError>>,
        verify_error: std::sync::Mutex<Option<ProposerError>>,
    }

    #[async_trait]
    impl OutputProposer for RecordingOutputProposer {
        async fn propose_output(
            &self,
            _proposal: &Proposal,
            _parent_address: Address,
            _intermediate_roots: &[B256],
        ) -> Result<(), ProposerError> {
            *self.created.lock().unwrap() += 1;
            if let Some(error) = self.create_error.lock().unwrap().take() {
                return Err(error);
            }
            Ok(())
        }

        async fn verify_proposal_proof(
            &self,
            game_address: Address,
            _proposal: &Proposal,
        ) -> Result<(), ProposerError> {
            self.verified.lock().unwrap().push(game_address);
            if let Some(error) = self.verify_error.lock().unwrap().take() {
                return Err(error);
            }
            Ok(())
        }
    }

    fn proof_result(target_block: u64) -> ProofResult {
        let mut aggregate_proposal = test_proposal(target_block);
        aggregate_proposal.l1_origin_hash = B256::ZERO;
        let proposals: Vec<Proposal> = (1..=target_block).map(test_proposal).collect();
        ProofResult::Tee { aggregate_proposal, proposals }
    }

    fn submitter(
        output_proposer: Arc<RecordingOutputProposer>,
        factory: MockDisputeGameFactory,
        verifier: MockAggregateVerifier,
    ) -> ProofSubmitter<MockL1, MockRollupClient> {
        submitter_with_factory(output_proposer, Arc::new(factory), verifier)
    }

    fn submitter_with_factory(
        output_proposer: Arc<RecordingOutputProposer>,
        factory: Arc<dyn DisputeGameFactoryClient>,
        verifier: MockAggregateVerifier,
    ) -> ProofSubmitter<MockL1, MockRollupClient> {
        let output_roots = HashMap::from([(TEST_BLOCK_INTERVAL, B256::repeat_byte(0x64))]);
        ProofSubmitter::new(
            output_proposer,
            Arc::new(MockRollupClient {
                sync_status: test_sync_status(TEST_BLOCK_INTERVAL, B256::ZERO),
                output_roots,
                max_safe_block: None,
            }),
            Arc::new(MockL1 { latest_block_number: 1000 }),
            factory,
            Arc::new(verifier),
            ProofSubmitterConfig {
                proposer_address: Address::repeat_byte(0x04),
                game_type: TEST_GAME_TYPE,
                block_interval: TEST_BLOCK_INTERVAL,
                intermediate_block_interval: TEST_BLOCK_INTERVAL,
                tee_image_hash: B256::repeat_byte(0x05),
                tee_prover_registry_address: None,
                output_fetch_concurrency: 1,
            },
        )
    }

    fn existing_game_factory(game_address: Address) -> MockDisputeGameFactory {
        let root = B256::repeat_byte(0x64);
        let extra_data = encode_extra_data(TEST_BLOCK_INTERVAL, Address::ZERO, &[root]);
        let mut factory = MockDisputeGameFactory::with_games(vec![]);
        factory.uuid_games.insert((TEST_GAME_TYPE, root, extra_data), game_address);
        factory
    }

    #[derive(Debug)]
    struct SequentialGameFactory {
        responses: std::sync::Mutex<VecDeque<Address>>,
    }

    impl SequentialGameFactory {
        fn new(responses: impl IntoIterator<Item = Address>) -> Self {
            Self { responses: std::sync::Mutex::new(responses.into_iter().collect()) }
        }
    }

    #[async_trait]
    impl DisputeGameFactoryClient for SequentialGameFactory {
        async fn game_count(&self) -> Result<u64, ContractError> {
            Ok(0)
        }

        async fn game_at_index(&self, index: u64) -> Result<GameAtIndex, ContractError> {
            Err(ContractError::Validation(format!("index {index} out of bounds")))
        }

        async fn init_bonds(&self, _: u32) -> Result<U256, ContractError> {
            Ok(U256::ZERO)
        }

        async fn game_impls(&self, _: u32) -> Result<Address, ContractError> {
            Ok(Address::ZERO)
        }

        async fn games(&self, _: u32, _: B256, _: Bytes) -> Result<Address, ContractError> {
            Ok(self.responses.lock().unwrap().pop_front().unwrap_or(Address::ZERO))
        }
    }

    #[tokio::test]
    async fn submit_attaches_proof_to_existing_matching_game() {
        let game_address = Address::repeat_byte(0xAA);
        let output = Arc::new(RecordingOutputProposer::default());
        let submitter = submitter(
            Arc::clone(&output),
            existing_game_factory(game_address),
            MockAggregateVerifier::default(),
        );

        let result = submitter
            .submit(&proof_result(TEST_BLOCK_INTERVAL), TEST_BLOCK_INTERVAL, Address::ZERO)
            .await;

        assert!(result.is_ok());
        assert_eq!(*output.created.lock().unwrap(), 0);
        assert_eq!(*output.verified.lock().unwrap(), vec![game_address]);
    }

    #[tokio::test]
    async fn submit_attaches_proof_after_game_already_exists_race() {
        let game_address = Address::repeat_byte(0xAA);
        let output = Arc::new(RecordingOutputProposer::default());
        *output.create_error.lock().unwrap() =
            Some(ProposerError::Submission(ProofSubmissionError::GameAlreadyExists));
        let submitter = submitter_with_factory(
            Arc::clone(&output),
            Arc::new(SequentialGameFactory::new([Address::ZERO, game_address])),
            MockAggregateVerifier::default(),
        );

        let result = submitter
            .submit(&proof_result(TEST_BLOCK_INTERVAL), TEST_BLOCK_INTERVAL, Address::ZERO)
            .await;

        assert!(result.is_ok());
        assert_eq!(*output.created.lock().unwrap(), 1);
        assert_eq!(*output.verified.lock().unwrap(), vec![game_address]);
    }

    #[tokio::test]
    async fn submit_recovers_when_existing_game_l1_head_mismatches() {
        let game_address = Address::repeat_byte(0xAA);
        let mut verifier = MockAggregateVerifier::default();
        verifier.l1_head_map.insert(game_address, B256::repeat_byte(0xCC));
        let output = Arc::new(RecordingOutputProposer::default());
        let submitter =
            submitter(Arc::clone(&output), existing_game_factory(game_address), verifier);

        let result = submitter
            .submit(&proof_result(TEST_BLOCK_INTERVAL), TEST_BLOCK_INTERVAL, Address::ZERO)
            .await;

        assert!(matches!(result, Err(SubmitAction::GameAlreadyExists)));
        assert_eq!(*output.created.lock().unwrap(), 0);
        assert!(output.verified.lock().unwrap().is_empty());
    }

    #[derive(Debug, Clone, Copy)]
    enum ExpectedAttachErrorAction {
        Success,
        Discard(&'static str),
    }

    #[rstest]
    #[case::already_verified(
        ProposerError::Submission(ProofSubmissionError::ProofAlreadyVerified),
        ExpectedAttachErrorAction::Success
    )]
    #[case::l1_origin_too_old(
        ProposerError::Submission(ProofSubmissionError::L1OriginTooOld),
        ExpectedAttachErrorAction::Discard("l1_origin_too_old")
    )]
    #[case::invalid_signer(
        ProposerError::Submission(ProofSubmissionError::InvalidSigner),
        ExpectedAttachErrorAction::Discard("invalid_signer")
    )]
    #[tokio::test]
    async fn submit_handles_existing_game_attach_error(
        #[case] error: ProposerError,
        #[case] expected: ExpectedAttachErrorAction,
    ) {
        let game_address = Address::repeat_byte(0xAA);
        let output = Arc::new(RecordingOutputProposer::default());
        *output.verify_error.lock().unwrap() = Some(error);
        let submitter = submitter(
            Arc::clone(&output),
            existing_game_factory(game_address),
            MockAggregateVerifier::default(),
        );

        let result = submitter
            .submit(&proof_result(TEST_BLOCK_INTERVAL), TEST_BLOCK_INTERVAL, Address::ZERO)
            .await;

        match expected {
            ExpectedAttachErrorAction::Success => assert!(result.is_ok()),
            ExpectedAttachErrorAction::Discard(label) => assert!(
                matches!(result, Err(SubmitAction::Discard(ref error)) if error.metric_label() == label)
            ),
        }
        assert_eq!(*output.created.lock().unwrap(), 0);
        assert_eq!(*output.verified.lock().unwrap(), vec![game_address]);
    }

    #[tokio::test]
    async fn submit_creates_game_when_no_match_exists() {
        let output = Arc::new(RecordingOutputProposer::default());
        let submitter = submitter(
            Arc::clone(&output),
            MockDisputeGameFactory::with_games(vec![]),
            MockAggregateVerifier::default(),
        );

        let result = submitter
            .submit(&proof_result(TEST_BLOCK_INTERVAL), TEST_BLOCK_INTERVAL, Address::ZERO)
            .await;

        assert!(result.is_ok());
        assert_eq!(*output.created.lock().unwrap(), 1);
        assert!(output.verified.lock().unwrap().is_empty());
    }
}
