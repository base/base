//! Main driver loop for the challenger service.
//!
//! The [`Driver`] ties together all challenger components — scanning for
//! invalid dispute games, validating output roots, requesting ZK proofs, and
//! submitting nullification transactions — into a single polling loop.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use alloy_primitives::{Address, B256};
use base_proof_contracts::AggregateVerifierClient;
use base_proof_rpc::L2Provider;
use base_tx_manager::TxManager;
use base_zk_client::{ProofType, ProveBlockRequest, ZkProofProvider};
use tokio::select;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::{
    CandidateGame, ChallengeSubmitter, ChallengerMetrics, GameScanner,
    IntermediateValidationParams, OutputValidator, PendingProof, PendingProofs, ProofPhase,
    ProofUpdate, ValidatorError,
};

/// Configuration for the challenger [`Driver`].
#[derive(Debug)]
pub struct DriverConfig {
    /// How often the driver polls for new games.
    pub poll_interval: Duration,
    /// Cancellation token for graceful shutdown.
    pub cancel: CancellationToken,
    /// Shared flag flipped to `true` after the first successful driver step.
    pub ready: Arc<AtomicBool>,
}

/// Orchestrates the challenger pipeline: scan, validate, prove, submit.
pub struct Driver<L2, P, T>
where
    L2: L2Provider,
    P: ZkProofProvider,
    T: TxManager,
{
    /// Scans for new dispute games on L1.
    pub scanner: GameScanner,
    /// Validates L2 output roots against the local node.
    pub validator: OutputValidator<L2>,
    /// ZK proof provider used to generate fault proofs.
    pub zk_prover: Arc<P>,
    /// Submits challenge transactions to L1.
    pub submitter: ChallengeSubmitter<T>,
    /// Client for the aggregate verifier contract.
    pub verifier_client: Arc<dyn AggregateVerifierClient>,
    /// In-flight proof sessions keyed by game address.
    pub pending_proofs: PendingProofs,
    /// Interval between polling cycles.
    pub poll_interval: Duration,
    /// Token used to signal graceful shutdown.
    pub cancel: CancellationToken,
    /// Indicates whether the driver has completed its first scan.
    pub ready: Arc<AtomicBool>,
    /// The last L1 block number that was scanned.
    pub last_scanned: Option<u64>,
}

impl<L2: L2Provider, P: ZkProofProvider, T: TxManager> std::fmt::Debug for Driver<L2, P, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Driver")
            .field("pending_proofs", &self.pending_proofs.len())
            .field("poll_interval", &self.poll_interval)
            .field("last_scanned", &self.last_scanned)
            .finish_non_exhaustive()
    }
}

impl<L2: L2Provider, P: ZkProofProvider, T: TxManager> Driver<L2, P, T> {
    /// Maximum number of times a failed proof job will be retried before being dropped.
    pub const MAX_PROOF_RETRIES: u32 = 3;

    /// Creates a new driver with the given components.
    pub fn new(
        config: DriverConfig,
        scanner: GameScanner,
        validator: OutputValidator<L2>,
        zk_prover: Arc<P>,
        submitter: ChallengeSubmitter<T>,
        verifier_client: Arc<dyn AggregateVerifierClient>,
    ) -> Self {
        Self {
            scanner,
            validator,
            zk_prover,
            submitter,
            verifier_client,
            pending_proofs: PendingProofs::new(),
            poll_interval: config.poll_interval,
            cancel: config.cancel,
            ready: config.ready,
            last_scanned: None,
        }
    }

    /// Runs the main driver loop until the cancellation token is fired.
    pub async fn run(mut self) {
        info!("challenger driver starting");
        let mut signalled_ready = false;
        loop {
            if self.cancel.is_cancelled() {
                info!("challenger driver shutting down");
                break;
            }

            match self.step().await {
                Ok(()) => {
                    if !signalled_ready {
                        signalled_ready = true;
                        self.ready.store(true, Ordering::SeqCst);
                        info!("service is ready");
                    }
                }
                Err(e) => {
                    warn!(error = %e, "driver step failed");
                }
            }

            metrics::gauge!(ChallengerMetrics::PENDING_PROOFS)
                .set(self.pending_proofs.len() as f64);

            select! {
                biased;
                () = self.cancel.cancelled() => {
                    info!("challenger driver shutting down");
                    break;
                }
                () = tokio::time::sleep(self.poll_interval) => {}
            }
        }
    }

    /// Executes a single scan-validate-prove-submit cycle.
    ///
    /// First polls any in-flight proof sessions that are not in the current
    /// scan batch, then scans for new candidates and processes them.
    pub async fn step(&mut self) -> eyre::Result<()> {
        // Poll in-flight proof sessions before scanning for new candidates.
        self.poll_pending_proofs().await;

        let (candidates, new_last_scanned) = self.scanner.scan(self.last_scanned).await?;
        self.last_scanned = new_last_scanned;

        for candidate in candidates {
            let index = candidate.index;
            if let Err(e) = self.process_candidate(candidate).await {
                warn!(error = %e, game_index = index, "failed to process candidate");
            }
        }

        Ok(())
    }

    /// Polls all in-flight proof sessions for completion or retries submission.
    async fn poll_pending_proofs(&mut self) {
        let addresses = self.pending_proofs.addresses();

        for game_address in addresses {
            if let Err(e) = self.poll_or_submit(game_address).await {
                warn!(
                    error = %e,
                    game = %game_address,
                    "failed to poll/submit pending proof"
                );
            }
        }
    }

    /// Processes a single candidate game: validate, prove if invalid, submit.
    async fn process_candidate(&mut self, candidate: CandidateGame) -> eyre::Result<()> {
        let game_address = candidate.factory.proxy;

        // If this game already has an in-flight proof session, skip it.
        // Pending proofs are polled separately in `poll_pending_proofs`.
        if self.pending_proofs.contains_key(&game_address) {
            debug!(game = %game_address, "skipping game with pending proof session");
            return Ok(());
        }

        let intermediate_roots =
            self.verifier_client.intermediate_output_roots(game_address).await?;

        let params = IntermediateValidationParams {
            game_address,
            starting_block_number: candidate.starting_block_number,
            l2_block_number: candidate.info.l2_block_number,
            intermediate_block_interval: candidate.intermediate_block_interval,
            claimed_root: candidate.info.root_claim,
            intermediate_roots,
        };

        let result = match self.validator.validate_intermediate_roots(params).await {
            Ok(r) => r,
            Err(e) => {
                match &e {
                    ValidatorError::BlockNotAvailable { .. } => {
                        debug!(
                            game = %game_address,
                            error = %e,
                            "block not yet available, skipping game"
                        );
                    }
                    _ => {
                        warn!(
                            game = %game_address,
                            error = %e,
                            "validation error, skipping game"
                        );
                    }
                }
                return Ok(());
            }
        };

        if result.is_valid {
            debug!(game = %game_address, "game output roots are valid");
            return Ok(());
        }

        let invalid_index =
            u64::try_from(result.invalid_intermediate_index.ok_or_else(|| {
                eyre::eyre!("invalid result missing invalid_intermediate_index")
            })?)?;
        let expected_root = result.expected_root;

        info!(
            game = %game_address,
            invalid_index = invalid_index,
            expected_root = %expected_root,
            "invalid intermediate root detected, requesting proof"
        );

        self.initiate_proof(candidate, invalid_index, expected_root).await
    }

    /// Requests a ZK proof, stores the session, and polls for the result.
    async fn initiate_proof(
        &mut self,
        candidate: CandidateGame,
        invalid_index: u64,
        expected_root: B256,
    ) -> eyre::Result<()> {
        let game_address = candidate.factory.proxy;

        // The prior intermediate root (or the game's starting root when
        // invalid_index == 0) is a trusted anchor, so the ZK proof only
        // needs to cover the single interval that contains the invalid
        // checkpoint: [prior_checkpoint .. invalid_checkpoint].
        let start_offset = candidate
            .intermediate_block_interval
            .checked_mul(invalid_index)
            .ok_or_else(|| eyre::eyre!("start_block_number offset overflow"))?;
        let start_block_number = candidate
            .starting_block_number
            .checked_add(start_offset)
            .ok_or_else(|| eyre::eyre!("start_block_number overflow"))?;

        let request = ProveBlockRequest {
            start_block_number,
            number_of_blocks_to_prove: candidate.intermediate_block_interval,
            sequence_window: None,
            proof_type: ProofType::GenericZkvmClusterCompressed as i32,
        };

        let prove_response = self.zk_prover.prove_block(request).await?;
        let session_id = prove_response.session_id;

        info!(
            game = %game_address,
            session_id = %session_id,
            "proof job initiated"
        );

        let pending = PendingProof::awaiting(session_id, invalid_index, expected_root, request);
        self.pending_proofs.insert(game_address, pending);

        if let Err(e) = self.poll_or_submit(game_address).await {
            warn!(error = %e, game = %game_address, "initial poll failed, will retry next tick");
        }

        Ok(())
    }

    /// Advances a pending proof through its lifecycle.
    ///
    /// - **`AwaitingProof`** — polls the ZK service:
    ///   - `Succeeded` → transitions to `ReadyToSubmit` and falls through to
    ///     submission.
    ///   - `Failed` → transitions to `NeedsRetry` so `prove_block` is
    ///     re-initiated.
    ///   - Intermediate (`Created`/`Pending`/`Running`) → returns early.
    /// - **`ReadyToSubmit`** — submits the nullification tx:
    ///   - On success → removes the entry.
    ///   - On failure → leaves the entry so it is retried next tick.
    /// - **`NeedsRetry`** — re-initiates `prove_block`:
    ///   - If `retry_count > MAX_PROOF_RETRIES` → drops the entry.
    ///   - Otherwise → calls `prove_block` and transitions to `AwaitingProof`.
    async fn poll_or_submit(&mut self, game_address: Address) -> eyre::Result<()> {
        let (invalid_index, expected_root) = match self.pending_proofs.get(&game_address) {
            Some(p) => (p.invalid_index, p.expected_root),
            None => return Ok(()),
        };

        // Check if the game is still challengeable before doing any work.
        let (status, zk_prover) = tokio::try_join!(
            self.verifier_client.status(game_address),
            self.verifier_client.zk_prover(game_address),
        )?;
        if status != GameScanner::STATUS_IN_PROGRESS {
            debug!(game = %game_address, status = status, "game no longer in progress, dropping pending proof");
            self.pending_proofs.remove(&game_address);
            return Ok(());
        }
        if zk_prover != Address::ZERO {
            debug!(game = %game_address, zk_prover = %zk_prover, "game already challenged, dropping pending proof");
            self.pending_proofs.remove(&game_address);
            return Ok(());
        }

        // Resolve the proof bytes — either by polling the ZK service or
        // extracting them from an already-obtained proof.
        let proof_bytes = match self.pending_proofs.poll(game_address, &*self.zk_prover).await? {
            Some(ProofUpdate::Ready(proof_bytes)) => {
                info!(
                    game = %game_address,
                    proof_len = proof_bytes.len(),
                    "proof ready, submitting nullification"
                );
                proof_bytes
            }
            Some(ProofUpdate::NeedsRetry) => {
                return self.handle_proof_retry(game_address).await;
            }
            Some(ProofUpdate::Pending) => {
                debug!(game = %game_address, "proof not ready, will retry next tick");
                return Ok(());
            }
            None => return Ok(()),
        };

        // ── Submit nullification ─────────────────────────────────────────
        match self
            .submitter
            .submit_nullification(game_address, proof_bytes, invalid_index, expected_root)
            .await
        {
            Ok(_) => {
                self.pending_proofs.remove(&game_address);
            }
            Err(e) => {
                warn!(
                    error = %e,
                    game = %game_address,
                    "nullification tx failed, will retry next tick"
                );
                // Leave entry as ReadyToSubmit for retry.
            }
        }

        Ok(())
    }

    /// Handles a proof that needs retrying after failure.
    ///
    /// If retries are exhausted the entry is dropped; otherwise `prove_block`
    /// is called and the phase transitions back to `AwaitingProof`.
    async fn handle_proof_retry(&mut self, game_address: Address) -> eyre::Result<()> {
        let pending = match self.pending_proofs.get(&game_address) {
            Some(p) => p,
            None => return Ok(()),
        };

        let retry_count = pending.retry_count;

        if retry_count > Self::MAX_PROOF_RETRIES {
            warn!(
                game = %game_address,
                retry_count = retry_count,
                "proof retries exhausted, dropping entry"
            );
            self.pending_proofs.remove(&game_address);
            return Ok(());
        }

        let request = pending.prove_request;

        metrics::counter!(ChallengerMetrics::PROOF_RETRIES_TOTAL).increment(1);

        match self.zk_prover.prove_block(request).await {
            Ok(response) => {
                info!(
                    game = %game_address,
                    session_id = %response.session_id,
                    retry_count = retry_count,
                    "proof job re-initiated"
                );
                if let Some(p) = self.pending_proofs.get_mut(&game_address) {
                    p.phase = ProofPhase::AwaitingProof { session_id: response.session_id };
                }
            }
            Err(e) => {
                if let Some(p) = self.pending_proofs.get_mut(&game_address) {
                    p.retry_count += 1;
                }
                warn!(
                    error = %e,
                    game = %game_address,
                    retry_count = retry_count,
                    "prove_block failed on retry, will retry next tick"
                );
                // Leave as NeedsRetry for next tick.
            }
        }

        Ok(())
    }
}
