//! Main driver loop for the challenger service.
//!
//! The [`Driver`] schedules game scanning, output validation, proof lifecycle
//! work, bond discovery, and anchor maintenance.

use std::{sync::Arc, time::Duration};

use base_proof_contracts::AggregateVerifierClient;
use base_proof_rpc::{L1Provider, L2Provider};
use base_prover_service_client::ProofRequesterProvider;
use base_runtime::TokioRuntime;
use base_tx_manager::TxManager;
use tokio::select;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::{
    AnchorUpdater, BondManager, CandidateGame, ChallengeSubmitter, ChallengerMetrics,
    DisputeIntent, DisputeProofManager, GameCategory, GameScanner, IntermediateValidationParams,
    OutputValidator, ValidatorError,
};

/// Configuration for the challenger [`Driver`].
#[derive(Debug)]
pub struct DriverConfig {
    /// How often the driver polls for new games.
    pub poll_interval: Duration,
    /// Cancellation token for graceful shutdown.
    pub cancel: CancellationToken,
}

/// Raw dependencies for the scan → validate → prove → dispute path.
///
/// The [`Driver`] takes these as `Option<DisputeComponents>`: `Some` when
/// disputing, `None` in no-dispute mode. [`Driver::new`] consumes them to build
/// the internal [`DisputeProofManager`]. The single `Option` records the mode
/// and keeps the proving dependencies all-present-or-all-absent — they cannot
/// disagree.
pub struct DisputeComponents<L2: L2Provider, P: ProofRequesterProvider> {
    /// Scans for new dispute games on L1.
    pub scanner: GameScanner,
    /// Validates L2 output roots against the local node.
    pub validator: OutputValidator<L2>,
    /// Prover-service requester used to generate and poll fault proofs (TEE and ZK).
    pub proof_requester: Arc<P>,
    /// L1 provider used to construct TEE proof requests.
    pub l1_provider: Arc<dyn L1Provider>,
    /// Maximum wall-clock time to wait for a proof session before treating it as failed.
    pub max_proof_duration: Duration,
    /// Retryable TEE submission failures to tolerate before falling back to ZK.
    pub tee_submit_retry_limit: u32,
}

impl<L2: L2Provider, P: ProofRequesterProvider> std::fmt::Debug for DisputeComponents<L2, P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DisputeComponents")
            .field("max_proof_duration", &self.max_proof_duration)
            .field("tee_submit_retry_limit", &self.tee_submit_retry_limit)
            .finish_non_exhaustive()
    }
}

/// Service-layer dependencies injected into the [`Driver`].
pub struct DriverComponents<L2: L2Provider, P: ProofRequesterProvider, T: TxManager> {
    /// Dispute-pipeline dependencies. `None` in no-dispute mode.
    pub dispute: Option<DisputeComponents<L2, P>>,
    /// Submits challenge transactions to L1 and bond/anchor maintenance transactions.
    pub submitter: ChallengeSubmitter<T>,
    /// Client for the aggregate verifier contract.
    pub verifier_client: Arc<dyn AggregateVerifierClient>,
    /// Bond lifecycle manager (optional; enabled when claim addresses are configured).
    pub bond_manager: Option<BondManager<TokioRuntime>>,
    /// Best-effort anchor state updater.
    pub anchor_updater: AnchorUpdater,
}

impl<L2: L2Provider, P: ProofRequesterProvider, T: TxManager> std::fmt::Debug
    for DriverComponents<L2, P, T>
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DriverComponents")
            .field("dispute", &self.dispute.as_ref().map(|_| ".."))
            .field("bond_manager", &self.bond_manager)
            .finish_non_exhaustive()
    }
}

/// Runtime state for the dispute pipeline: the [`DisputeProofManager`] built by
/// [`Driver::new`], plus the scanner and validator the [`Driver`] drives
/// directly. Held by the driver as `Option<DisputePipeline>` — `None` in
/// no-dispute mode.
pub struct DisputePipeline<L2: L2Provider, P: ProofRequesterProvider> {
    /// Manages proof sessions, retries, submissions, and TEE-to-ZK fallback.
    pub proof_manager: DisputeProofManager<L2, P>,
    /// Scans for new dispute games on L1.
    pub scanner: GameScanner,
    /// Validates L2 output roots against the local node.
    pub validator: OutputValidator<L2>,
}

impl<L2: L2Provider, P: ProofRequesterProvider> std::fmt::Debug for DisputePipeline<L2, P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DisputePipeline").finish_non_exhaustive()
    }
}

/// Orchestrates the challenger: the bond/anchor lifecycle always runs, plus the
/// dispute pipeline (scan → validate → prove → dispute) when `dispute` is
/// `Some`. In no-dispute mode `dispute` is `None` and the pipeline is skipped
/// entirely.
pub struct Driver<L2, P, T>
where
    L2: L2Provider,
    P: ProofRequesterProvider,
    T: TxManager,
{
    /// Dispute-pipeline state. `None` in no-dispute mode; when `None`, the
    /// driver runs only the bond/anchor lifecycle.
    pub dispute: Option<DisputePipeline<L2, P>>,
    /// Submits challenge transactions to L1 and bond/anchor maintenance transactions.
    pub submitter: ChallengeSubmitter<T>,
    /// Client for the aggregate verifier contract.
    pub verifier_client: Arc<dyn AggregateVerifierClient>,
    /// Bond lifecycle manager (optional; enabled when claim addresses are configured).
    pub bond_manager: Option<BondManager<TokioRuntime>>,
    /// Best-effort anchor state updater.
    pub anchor_updater: AnchorUpdater,
    /// Interval between polling cycles.
    pub poll_interval: Duration,
    /// Token used to signal graceful shutdown.
    pub cancel: CancellationToken,
}

impl<L2: L2Provider, P: ProofRequesterProvider, T: TxManager> std::fmt::Debug for Driver<L2, P, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Driver")
            .field("dispute", &self.dispute.as_ref().map(|_| ".."))
            .field("poll_interval", &self.poll_interval)
            .finish_non_exhaustive()
    }
}

impl<L2: L2Provider, P: ProofRequesterProvider, T: TxManager> Driver<L2, P, T> {
    /// Creates a new driver from its configuration and components.
    ///
    /// When `components.dispute` is `Some`, the proving dependencies are
    /// consumed to build the internal [`DisputeProofManager`]. When `None`, the
    /// driver runs only the bond/anchor lifecycle (no-dispute mode).
    pub fn new(config: DriverConfig, components: DriverComponents<L2, P, T>) -> Self {
        let dispute = components.dispute.map(|dispute| DisputePipeline {
            proof_manager: DisputeProofManager::new(
                dispute.validator.clone(),
                dispute.proof_requester,
                dispute.l1_provider,
                Arc::clone(&components.verifier_client),
                dispute.max_proof_duration,
                dispute.tee_submit_retry_limit,
            ),
            scanner: dispute.scanner,
            validator: dispute.validator,
        });

        Self {
            dispute,
            submitter: components.submitter,
            verifier_client: components.verifier_client,
            bond_manager: components.bond_manager,
            anchor_updater: components.anchor_updater,
            poll_interval: config.poll_interval,
            cancel: config.cancel,
        }
    }

    /// Runs the main driver loop until the cancellation token is fired.
    pub async fn run(mut self) {
        info!(no_dispute = self.dispute.is_none(), "challenger driver starting");
        while !self.cancel.is_cancelled() {
            if let Err(e) = self.step().await {
                warn!(error = %e, "driver step failed");
            }

            if let Some(dispute) = &self.dispute {
                ChallengerMetrics::pending_proofs()
                    .set(dispute.proof_manager.pending_proofs_len() as f64);
                ChallengerMetrics::ignored_games()
                    .set(dispute.proof_manager.ignored_games_len() as f64);
            }

            select! {
                () = self.cancel.cancelled() => break,
                () = tokio::time::sleep(self.poll_interval) => {}
            }
        }
        info!("challenger driver shutting down");
    }

    /// Executes a single cycle: advance in-flight proofs (when disputing), run
    /// the bond/anchor lifecycle, then scan for and process candidates.
    ///
    /// In no-dispute mode (`dispute` is `None`) only the bond/anchor lifecycle
    /// runs — no scanning, validation, proving, or dispute submission.
    pub async fn step(&mut self) -> eyre::Result<()> {
        if let Some(dispute) = &mut self.dispute {
            dispute.proof_manager.poll_pending_proofs(&self.submitter).await;
        }

        if let Some(bond_manager) = &mut self.bond_manager
            && let Err(e) =
                bond_manager.discover_claimable_games(&*self.verifier_client, &self.submitter).await
        {
            warn!(error = %e, "bond discovery scan failed");
        }
        self.anchor_updater.poll(&*self.verifier_client, &self.submitter).await;

        let candidates = match &mut self.dispute {
            Some(dispute) => dispute.scanner.scan().await?,
            None => return Ok(()),
        };

        for candidate in candidates {
            let index = candidate.index;
            if let Err(e) = self.process_candidate(candidate).await {
                warn!(error = %e, game_index = index, "failed to process candidate");
            }
        }

        Ok(())
    }

    /// Processes a candidate game according to its [`GameCategory`].
    async fn process_candidate(&mut self, candidate: CandidateGame) -> eyre::Result<()> {
        let game_address = candidate.factory.proxy;

        {
            let proof_manager = &self
                .dispute
                .as_ref()
                .expect("dispute components must be set unless --no-dispute is set")
                .proof_manager;

            if proof_manager.is_ignored(game_address) {
                debug!(game = %game_address, "skipping ignored game");
                return Ok(());
            }

            if proof_manager.has_pending_proof(game_address) {
                debug!(game = %game_address, "skipping game with pending proof session");
                return Ok(());
            }
        }

        match candidate.category {
            GameCategory::InvalidTeeProposal => {
                self.process_invalid_proposal(candidate, DisputeIntent::Challenge, true).await
            }
            GameCategory::FraudulentZkChallenge { challenged_index } => {
                self.process_fraudulent_zk_challenge(candidate, challenged_index).await
            }
            GameCategory::InvalidZkProposal => {
                self.process_invalid_proposal(candidate, DisputeIntent::Nullify, false).await
            }
            GameCategory::InvalidDualProposal => {
                self.process_invalid_proposal(candidate, DisputeIntent::Nullify, true).await
            }
        }
    }

    /// Fetches intermediate roots and validates them against the local L2 node.
    ///
    /// Returns `Ok(Some(result))` when validation completes, or `Ok(None)`
    /// when a transient error means the game should be retried next tick.
    async fn validate_game(
        &self,
        candidate: &CandidateGame,
    ) -> eyre::Result<Option<crate::ValidationResult>> {
        let game_address = candidate.factory.proxy;

        let intermediate_roots =
            self.verifier_client.intermediate_output_roots(game_address).await?;

        let params = IntermediateValidationParams {
            game_address,
            starting_block_number: candidate.starting_block_number,
            l2_block_number: candidate.info.l2_block_number,
            intermediate_block_interval: candidate.intermediate_block_interval,
            claimed_root: candidate.info.root_claim,
            intermediate_roots: &intermediate_roots,
        };

        match self
            .dispute
            .as_ref()
            .expect("dispute components must be set unless --no-dispute is set")
            .validator
            .validate_intermediate_roots(params)
            .await
        {
            Ok(result) => Ok(Some(result)),
            Err(e) => match &e {
                ValidatorError::BlockNotAvailable { .. } => {
                    debug!(
                        game = %game_address,
                        error = %e,
                        "block not yet available, skipping game"
                    );
                    Ok(None)
                }
                ValidatorError::CheckpointCountMismatch { .. }
                | ValidatorError::InvalidInterval
                | ValidatorError::InvalidBlockRange { .. } => Err(e.into()),
                _ => {
                    warn!(
                        game = %game_address,
                        error = %e,
                        "transient validation error, skipping game"
                    );
                    Ok(None)
                }
            },
        }
    }

    /// Validates an invalid proposal and starts its proof lifecycle.
    async fn process_invalid_proposal(
        &mut self,
        candidate: CandidateGame,
        intent: DisputeIntent,
        try_tee_first: bool,
    ) -> eyre::Result<()> {
        let game_address = candidate.factory.proxy;

        let result = match self.validate_game(&candidate).await? {
            Some(result) => result,
            None => return Ok(()),
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
            intent = ?intent,
            "invalid intermediate root detected, requesting proof"
        );

        let metric = match &candidate.category {
            GameCategory::InvalidTeeProposal => {
                ChallengerMetrics::invalid_tee_proposal_detected_total()
            }
            GameCategory::InvalidZkProposal => {
                ChallengerMetrics::invalid_zk_proposal_detected_total()
            }
            GameCategory::InvalidDualProposal => {
                ChallengerMetrics::invalid_dual_proposal_detected_total()
            }
            GameCategory::FraudulentZkChallenge { .. } => {
                error!(
                    category = ?candidate.category,
                    game = %game_address,
                    "unexpected category in process_invalid_proposal"
                );
                debug_assert!(
                    false,
                    "unexpected category in process_invalid_proposal: {:?}",
                    candidate.category
                );
                return Err(eyre::eyre!(
                    "unexpected category in process_invalid_proposal: {:?}",
                    candidate.category
                ));
            }
        };
        metric.increment(1);

        let prover_address = self.submitter.sender_address();
        self.dispute
            .as_mut()
            .expect("dispute components must be set unless --no-dispute is set")
            .proof_manager
            .initiate_proof(
                prover_address,
                candidate,
                invalid_index,
                expected_root,
                intent,
                try_tee_first,
            )
            .await
    }

    /// Validates a challenged TEE root and nullifies a fraudulent ZK challenge.
    async fn process_fraudulent_zk_challenge(
        &mut self,
        candidate: CandidateGame,
        challenged_index: u64,
    ) -> eyre::Result<()> {
        let game_address = candidate.factory.proxy;
        let on_chain_root =
            self.verifier_client.intermediate_output_root(game_address, challenged_index).await?;
        let checkpoint_block = candidate.checkpoint_start_block(challenged_index + 1)?;

        let validation = match self
            .dispute
            .as_ref()
            .expect("dispute components must be set unless --no-dispute is set")
            .validator
            .validate_claimed_root_at_block(game_address, checkpoint_block, on_chain_root)
            .await
        {
            Ok(result) => result,
            Err(ValidatorError::BlockNotAvailable { .. }) => {
                debug!(
                    game = %game_address,
                    block = checkpoint_block,
                    "block not yet available, skipping game"
                );
                return Ok(());
            }
            Err(e) => {
                warn!(
                    game = %game_address,
                    block = checkpoint_block,
                    error = %e,
                    "output root computation failed, skipping game"
                );
                return Ok(());
            }
        };

        if !validation.is_valid {
            debug!(
                game = %game_address,
                challenged_index = challenged_index,
                on_chain = %on_chain_root,
                expected = %validation.expected_root,
                "ZK challenge is legitimate (challenged root was wrong), skipping"
            );
            return Ok(());
        }

        info!(
            game = %game_address,
            challenged_index = challenged_index,
            on_chain_root = %on_chain_root,
            "fraudulent ZK challenge detected, nullifying with ZK proof"
        );
        ChallengerMetrics::fraudulent_zk_challenge_detected_total().increment(1);

        let prover_address = self.submitter.sender_address();
        self.dispute
            .as_mut()
            .expect("dispute components must be set unless --no-dispute is set")
            .proof_manager
            .initiate_zk_proof(
                prover_address,
                candidate,
                challenged_index,
                validation.expected_root,
                DisputeIntent::Nullify,
            )
            .await
    }
}
