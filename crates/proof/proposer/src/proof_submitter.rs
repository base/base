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
//! 3. Optional pre-submission TEE signer validation against the on-chain
//!    `TEEProverRegistry`.
//! 4. Calls `output_proposer.propose_output(..)` to create the dispute game on
//!    L1 and maps contract-level errors into the [`SubmitAction`] variants the
//!    pipeline interprets.
//!
//! Each [`ProofSubmitter::submit`] call is independent and reads no pipeline
//! state. The pipeline enforces single-flight submission via its own join set,
//! while the submitter focuses on per-proposal validation and the L1 call.

use std::{collections::HashMap, sync::Arc};

use alloy_primitives::{Address, B256, Signature, keccak256};
use alloy_sol_types::SolCall;
use base_proof_contracts::ITEEProverRegistry;
use base_proof_primitives::{ProofJournal, ProofResult, Proposal};
use base_proof_rpc::{L1Provider, RollupProvider};
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
    /// The dispute game already exists on-chain by a previous attempt whose
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
    /// Number of L2 blocks per proposal.
    pub block_interval: u64,
    /// Stride (in L2 blocks) between intermediate roots within a proposal.
    pub intermediate_block_interval: u64,
    /// Expected TEE enclave image hash.
    pub tee_image_hash: B256,
    /// Optional on-chain `TEEProverRegistry` address. When set, the submitter
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
        config: ProofSubmitterConfig,
    ) -> Self {
        Self { output_proposer, rollup_client, l1_client, config }
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
        // and check `isValidSigner` on-chain. If the signer is invalid, skip
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
                    // The proof's signer is not registered on-chain. Discard
                    // this proof so the pipeline re-proves with a (potentially
                    // different, registered) enclave on the next attempt.
                    warn!(target_block, "TEE signer is not valid on-chain, discarding proof");
                    Metrics::tee_signer_invalid_total().increment(1);
                    return Err(SubmitAction::Discard(ProposerError::Internal(
                        "TEE signer not registered on-chain".into(),
                    )));
                }
                Err(e) => {
                    // Proceed on RPC failure: if L1 is unreachable, the
                    // subsequent propose_output call will also fail and be
                    // retried naturally. Blocking here would not save gas.
                    // This also handles the case where the registry contract
                    // is not yet deployed (rolling out the
                    // --tee-prover-registry-address config before the contract
                    // exists on-chain).
                    warn!(error = %e, target_block, "signer validity check failed, proceeding anyway");
                }
            }
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
                if e.is_game_already_exists() {
                    drop(propose_timer);
                    info!(
                        target_block,
                        "Game already exists, next tick will load fresh state from chain"
                    );
                    Err(SubmitAction::GameAlreadyExists)
                } else if e.is_l1_origin_too_old() {
                    propose_timer.disarm();
                    warn!(
                        error = %e,
                        target_block,
                        "Proof L1 origin is too old, discarding proof to re-prove"
                    );
                    Err(SubmitAction::Discard(e))
                } else if e.is_invalid_signer() {
                    propose_timer.disarm();
                    warn!(
                        error = %e,
                        target_block,
                        "Proof signer is invalid on-chain, discarding proof to re-prove"
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
            .call_contract(registry_address, calldata.into(), None)
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
