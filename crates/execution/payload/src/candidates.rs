//! Builder-side candidate-group selection for the native payload builder (design-v2 §5).
//!
//! When the pool iterator yields a representative carrying a [`CandidateGroup`], the builder
//! applies the normative lifecycle precedence ([`GroupLifecycle`]), runs every eligible sibling
//! as an uncommitted trial at the frozen prefix, scores each trial with score-read context v1,
//! and commits at most one sibling (the winner, or the signed default sibling as fallback)
//! through the ordinary resource-checked commit path. Park, defer and evict never mark the
//! nonce committed.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use alloy_consensus::Transaction;
use alloy_evm::{
    Evm as AlloyEvm, EvmError,
    block::{CommitChanges, TxResult},
};
use alloy_primitives::{Address, B256, TxKind, U256};
use base_common_chains::Upgrades;
use base_common_consensus::{BaseTransactionSigned, TxDeposit};
use base_execution_candidates::{
    AccessDenied, AttemptId, CandidateGroup, CandidateRuntime, CandidateSelectionRule,
    CandidateSibling, DEFAULT_MAX_EVALUATIONS, DEFAULT_PROBE_GAS, DeferReason, EvictionReason,
    FallbackMode, FallbackReason, GroupLifecycle, Ineligibility, ParkReason, ProbeResult,
    ReserveError, ScoreRead, TrialOutcome, TurnContext, TurnDecision, WinnerSelection,
};
use base_execution_txpool::{BasePooledTransaction, estimated_da_size::DataAvailabilitySized};
use reth_chainspec::EthChainSpec;
use reth_evm::{
    ConfigureEvm, Database, EvmEnvFor,
    execute::{BlockBuilder, BlockExecutionError, BlockExecutor, BlockValidationError},
};
use reth_payload_builder_primitives::PayloadBuilderError;
use reth_payload_primitives::BuildNextEnv;
use reth_primitives_traits::{HeaderTy, Recovered, TxTy, WithEncoded};
use reth_transaction_pool::PoolTransaction;
use revm::{
    Database as _,
    context::result::{ExecutionResult, ResultAndState},
    state::{Account, AccountInfo, Bytecode, EvmState},
};
use tracing::{debug, trace};

use crate::{
    Attributes, BuilderMetrics, ParkablePayloadTransactions, PayloadPrimitives,
    builder::{BasePayloadBuilderCtx, ExecutionInfo},
};

/// Default per-build wall-time budget for candidate handling.
pub const DEFAULT_CANDIDATE_EVAL_WALL_CUTOFF: Duration = Duration::from_millis(10);

/// Candidate selection settings for the native builder.
#[derive(Debug, Clone)]
pub struct CandidateBuilderConfig {
    /// Shared ledger, policy and kill switch.
    pub runtime: Arc<CandidateRuntime>,
    /// Gas cap for each score-read probe (`G_probe`).
    pub probe_gas: u64,
    /// Wall time candidate handling may spend per payload build.
    pub eval_wall_cutoff: Duration,
    /// Server cap on evaluations per group, applied below the signed `maxEvaluations`.
    pub server_max_evaluations: u16,
}

impl CandidateBuilderConfig {
    /// Creates a configuration with default probe gas, wall cutoff and evaluation cap.
    pub const fn new(runtime: Arc<CandidateRuntime>) -> Self {
        Self {
            runtime,
            probe_gas: DEFAULT_PROBE_GAS,
            eval_wall_cutoff: DEFAULT_CANDIDATE_EVAL_WALL_CUTOFF,
            server_max_evaluations: DEFAULT_MAX_EVALUATIONS,
        }
    }
}

/// Block-level limits a candidate sibling must fit, captured once per scan.
#[derive(Debug, Clone, Copy)]
pub struct CandidateBlockLimits {
    /// Block number being built.
    pub block_number: u64,
    /// Effective block gas limit.
    pub block_gas_limit: u64,
    /// Per-transaction DA limit.
    pub tx_da_limit: Option<u64>,
    /// Per-block DA limit.
    pub block_da_limit: Option<u64>,
    /// DA footprint gas scalar, after Jovian.
    pub da_footprint_gas_scalar: Option<u16>,
    /// Block base fee.
    pub base_fee: u64,
}

/// Per-build candidate state carried across turns of one scan.
#[derive(Debug, Default)]
pub struct CandidateScan {
    /// Wall time spent handling groups in this build.
    pub eval_duration: Duration,
}

/// Discarded overlay over the frozen prefix for score-read probes (score-read context v1).
///
/// With a diff, reads resolve to the prefix plus only that trial's state diff, applying
/// revm's created, self-destructed and storage semantics; without one, reads are the prefix.
/// Writes made by a probe stay in the probe EVM's journal and are dropped with it.
#[derive(Debug)]
pub struct ScoreOverlay<'a, DB> {
    inner: &'a mut DB,
    diff: Option<&'a EvmState>,
}

impl<'a, DB> ScoreOverlay<'a, DB> {
    /// Creates an overlay over `inner`, optionally applying a trial's state diff.
    pub const fn new(inner: &'a mut DB, diff: Option<&'a EvmState>) -> Self {
        Self { inner, diff }
    }

    fn touched(&self, address: &Address) -> Option<&'a Account> {
        self.diff.and_then(|diff| diff.get(address)).filter(|account| account.is_touched())
    }
}

impl<DB: Database> revm::Database for ScoreOverlay<'_, DB> {
    type Error = DB::Error;

    fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        match self.touched(&address) {
            Some(account) if account.is_selfdestructed() => Ok(None),
            Some(account) => Ok(Some(account.info.clone())),
            None => self.inner.basic(address),
        }
    }

    fn code_by_hash(&mut self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        let from_diff = self.diff.and_then(|diff| {
            diff.values().find_map(|account| {
                (account.info.code_hash == code_hash).then(|| account.info.code.clone()).flatten()
            })
        });
        match from_diff {
            Some(code) => Ok(code),
            None => self.inner.code_by_hash(code_hash),
        }
    }

    fn storage(&mut self, address: Address, index: U256) -> Result<U256, Self::Error> {
        if let Some(account) = self.touched(&address) {
            if account.is_selfdestructed() {
                return Ok(U256::ZERO);
            }
            if let Some(slot) = account.storage.get(&index) {
                return Ok(slot.present_value);
            }
            if account.is_created() {
                return Ok(U256::ZERO);
            }
        }
        self.inner.storage(address, index)
    }

    fn block_hash(&mut self, number: u64) -> Result<B256, Self::Error> {
        self.inner.block_hash(number)
    }
}

/// One group's turn: the group, the block limits and the candidate configuration.
#[derive(Debug, Clone, Copy)]
pub struct CandidateTurn<'a> {
    /// The group carried by the iterator's current representative.
    pub group: &'a Arc<CandidateGroup>,
    /// Limits of the block being built.
    pub limits: &'a CandidateBlockLimits,
    /// Candidate selection settings.
    pub config: &'a CandidateBuilderConfig,
}

/// How committing one sibling through the resource-checked path ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SiblingCommit {
    /// Committed; the lane is released.
    Committed,
    /// Executed resource admission refused the commit.
    Excluded {
        /// Whether the exclusion is transaction-scope.
        permanent: bool,
    },
    /// The shared nonce is already used.
    NonceTooLow,
    /// The sibling failed EVM validation at this prefix.
    Invalid,
}

/// A sibling's uncommitted trial.
#[derive(Debug)]
pub struct SiblingTrial {
    /// Gas used, or why the trial is ineligible before scoring.
    pub outcome: Result<u64, Ineligibility>,
    /// The trial's state diff, kept only when a token probe needs it.
    pub state: Option<EvmState>,
    /// The recipient's post-trial native balance, when the trial touched it.
    pub recipient_balance: Option<U256>,
}

impl SiblingTrial {
    const fn invalid() -> Self {
        Self { outcome: Err(Ineligibility::Invalid), state: None, recipient_balance: None }
    }
}

impl<Evm, ChainSpec, Attrs> BasePayloadBuilderCtx<Evm, ChainSpec, Attrs>
where
    Evm: ConfigureEvm<
            Primitives: PayloadPrimitives,
            NextBlockEnvCtx: BuildNextEnv<Attrs, HeaderTy<Evm::Primitives>, ChainSpec>,
        >,
    ChainSpec: EthChainSpec + Upgrades,
    Attrs: Attributes<Transaction = TxTy<Evm::Primitives>>,
{
    /// Handles the iterator's current representative, which carries `group`.
    ///
    /// Without a candidate configuration the group is parked: a builder never executes a
    /// representative as a plain transaction. Returns `Ok(Some(()))` if the job was cancelled.
    pub fn execute_candidate_group<Builder, B>(
        &self,
        info: &mut ExecutionInfo,
        builder: &mut Builder,
        best_txs: &mut B,
        group: &Arc<CandidateGroup>,
        limits: &CandidateBlockLimits,
        scan: &mut CandidateScan,
    ) -> Result<Option<()>, PayloadBuilderError>
    where
        Builder: BlockBuilder<Primitives = Evm::Primitives>,
        <<Builder::Executor as BlockExecutor>::Evm as AlloyEvm>::DB: Database,
        B: ParkablePayloadTransactions<Transaction: PoolTransaction>,
    {
        let Some(config) = &self.builder_config.candidates else {
            trace!(
                target: "payload_builder",
                group_hash = %group.group_hash(),
                "parking candidate group: candidate selection is not configured"
            );
            best_txs.park_current();
            return Ok(None);
        };
        let started = Instant::now();
        let turn = CandidateTurn { group, limits, config };
        let result = self.candidate_turn(info, builder, best_txs, turn, scan);
        scan.eval_duration += started.elapsed();
        result
    }

    fn candidate_turn<Builder, B>(
        &self,
        info: &mut ExecutionInfo,
        builder: &mut Builder,
        best_txs: &mut B,
        turn: CandidateTurn<'_>,
        scan: &CandidateScan,
    ) -> Result<Option<()>, PayloadBuilderError>
    where
        Builder: BlockBuilder<Primitives = Evm::Primitives>,
        <<Builder::Executor as BlockExecutor>::Evm as AlloyEvm>::DB: Database,
        B: ParkablePayloadTransactions<Transaction: PoolTransaction>,
    {
        let group = turn.group;
        let limits = turn.limits;
        let config = turn.config;
        let runtime = &config.runtime;
        let group_hash = group.group_hash();
        let entry = runtime.ledger.lookup(&group_hash).filter(|entry| !entry.state.is_terminal());
        let principal_authorized = entry
            .as_ref()
            .is_some_and(|entry| runtime.policy.principal(group.sender()) == Some(entry.principal));
        let all: Vec<usize> = (0..group.siblings().len()).collect();
        let full_round_gas = Self::round_gas(group, &all, config);
        let decision = GroupLifecycle::before_evaluation(&TurnContext {
            target_block: limits.block_number,
            expiry_block: group.envelope().expiry_block,
            ledger_available: entry.is_some(),
            principal_authorized,
            selection_enabled: runtime.switch.is_enabled(),
            fallback: group.fallback(),
            group_budget_exhausted: entry.as_ref().is_some_and(|entry| {
                entry.budget_exhausted(full_round_gas, config.server_max_evaluations)
            }),
            build_budget_available: scan.eval_duration < config.eval_wall_cutoff,
        });
        let Some(entry) = entry else {
            return Ok(self.apply_decision(info, best_txs, group, decision, config));
        };
        if decision != TurnDecision::Evaluate {
            return self.decide(info, builder, best_txs, turn, decision);
        }

        // Per-sibling static and simulated admission at the unchanged prefix; nothing reserved.
        let mut outcomes: Vec<TrialOutcome> = Vec::with_capacity(group.siblings().len());
        let mut round = Vec::new();
        for (index, sibling) in group.siblings().iter().enumerate() {
            let check = if entry.permanently_ineligible.get(index).copied().unwrap_or(false) {
                Err(Ineligibility::PermanentlyExcluded)
            } else {
                self.sibling_precheck(info, sibling, limits)
            };
            match check {
                Ok(()) => {
                    round.push(index);
                    outcomes.push(TrialOutcome::Ineligible(Ineligibility::NotRun));
                }
                Err(reason) => outcomes.push(TrialOutcome::Ineligible(reason)),
            }
        }
        self.record_permanent_exclusions(group, &outcomes, config);
        if round.is_empty() {
            let decision = Self::without_trials(&outcomes, group.fallback());
            return self.decide(info, builder, best_txs, turn, decision);
        }

        // Reserve principal work first so a quota failure consumes no attempt.
        let round_gas = Self::round_gas(group, &round, config);
        let attempt = AttemptId {
            group_hash,
            parent_hash: self.parent().hash(),
            seq: entry.attempts.saturating_add(1),
        };
        let ticket = match runtime.policy.reserve(entry.principal, attempt, round_gas) {
            Ok(ticket) => ticket,
            Err(AccessDenied::NotAllowed) => {
                let decision = TurnDecision::Evict(EvictionReason::Revoked);
                return Ok(self.apply_decision(info, best_txs, group, decision, config));
            }
            Err(_) => {
                let decision = TurnDecision::Defer(DeferReason::PrincipalQuota);
                return Ok(self.apply_decision(info, best_txs, group, decision, config));
            }
        };
        let reservation =
            match runtime.ledger.reserve(group_hash, round_gas, config.server_max_evaluations) {
                Ok(reservation) => reservation,
                Err(error) => {
                    runtime.policy.settle(ticket, 0);
                    let decision = match error {
                        ReserveError::Unknown => TurnDecision::Park(ParkReason::LedgerUnavailable),
                        ReserveError::Exhausted => match group.fallback() {
                            FallbackMode::DefaultSibling => {
                                TurnDecision::Fallback(FallbackReason::BudgetExhausted)
                            }
                            FallbackMode::Drop => {
                                TurnDecision::Evict(EvictionReason::BudgetExhausted)
                            }
                        },
                    };
                    return self.decide(info, builder, best_txs, turn, decision);
                }
            };

        let evaluation = self.evaluate_round(
            &info.resource_metering_usage,
            builder,
            group,
            &round,
            &mut outcomes,
            config,
        );
        let work = match &evaluation {
            Ok(Some(work)) => *work,
            Ok(None) | Err(_) => reservation.gas,
        };
        runtime.ledger.settle(reservation, work);
        runtime.policy.settle(ticket, work);
        if evaluation?.is_none() {
            return Ok(Some(()));
        }
        if let Some(nonce_used) = outcomes
            .iter()
            .position(|outcome| matches!(outcome, TrialOutcome::Ineligible(Ineligibility::Invalid)))
            && self.nonce_already_used(builder, group)
        {
            debug!(
                target: "payload_builder",
                group_hash = %group_hash,
                sibling = nonce_used,
                "candidate group nonce already used"
            );
            runtime.ledger.evict(group_hash, EvictionReason::Dropped);
            best_txs.mark_current_committed();
            return Ok(None);
        }
        self.record_permanent_exclusions(group, &outcomes, config);

        let winner = WinnerSelection::choose(&outcomes);
        let decision = match winner {
            Some(_) => TurnDecision::CommitWinner,
            None if outcomes.iter().all(|outcome| match outcome {
                TrialOutcome::Ineligible(reason) => reason.is_block_scope(),
                TrialOutcome::Eligible { .. } => false,
            }) =>
            {
                TurnDecision::Defer(DeferReason::BlockLimits)
            }
            None => {
                let budget_remains = runtime.ledger.lookup(&group_hash).is_some_and(|entry| {
                    !entry.budget_exhausted(full_round_gas, config.server_max_evaluations)
                });
                GroupLifecycle::after_evaluation(false, group.fallback(), budget_remains)
            }
        };
        debug!(
            target: "payload_builder",
            group_hash = %group_hash,
            attempt = attempt.seq,
            work_gas = work,
            winner = ?winner,
            decision = decision.as_label(),
            "evaluated candidate group"
        );
        match (decision, winner) {
            (TurnDecision::CommitWinner, Some(index)) => {
                match self.commit_sibling(info, builder, group, index, limits)? {
                    SiblingCommit::Committed => {
                        Self::landed(group, index, limits, config);
                        best_txs.mark_current_committed();
                    }
                    SiblingCommit::NonceTooLow => {
                        runtime.ledger.evict(group_hash, EvictionReason::Dropped);
                        best_txs.mark_current_committed();
                    }
                    // The live re-execution disagreed with the trial at the same prefix.
                    SiblingCommit::Excluded { .. } | SiblingCommit::Invalid => {
                        let decision = TurnDecision::Defer(DeferReason::VerifyMismatch);
                        self.apply_decision(info, best_txs, group, decision, config);
                    }
                }
                Ok(None)
            }
            (decision, _) => self.decide(info, builder, best_txs, turn, decision),
        }
    }

    /// Applies a decision that may run the fallback sibling.
    fn decide<Builder, B>(
        &self,
        info: &mut ExecutionInfo,
        builder: &mut Builder,
        best_txs: &mut B,
        turn: CandidateTurn<'_>,
        decision: TurnDecision,
    ) -> Result<Option<()>, PayloadBuilderError>
    where
        Builder: BlockBuilder<Primitives = Evm::Primitives>,
        <<Builder::Executor as BlockExecutor>::Evm as AlloyEvm>::DB: Database,
        B: ParkablePayloadTransactions<Transaction: PoolTransaction>,
    {
        let group = turn.group;
        let limits = turn.limits;
        let config = turn.config;
        let TurnDecision::Fallback(reason) = decision else {
            return Ok(self.apply_decision(info, best_txs, group, decision, config));
        };
        if self.cancel.is_cancelled() {
            return Ok(Some(()));
        }
        let index = usize::from(group.envelope().default_index);
        let decision = match self.sibling_precheck(info, &group.siblings()[index], limits) {
            Err(reason) if reason.is_permanent() => {
                TurnDecision::Evict(EvictionReason::PermanentExclusion)
            }
            Err(_) => TurnDecision::Defer(DeferReason::BlockLimits),
            Ok(()) => match self.commit_sibling(info, builder, group, index, limits)? {
                SiblingCommit::Committed => {
                    debug!(
                        target: "payload_builder",
                        group_hash = %group.group_hash(),
                        reason = ?reason,
                        "committed candidate fallback sibling"
                    );
                    Self::landed(group, index, limits, config);
                    best_txs.mark_current_committed();
                    return Ok(None);
                }
                SiblingCommit::NonceTooLow => {
                    config.runtime.ledger.evict(group.group_hash(), EvictionReason::Dropped);
                    best_txs.mark_current_committed();
                    return Ok(None);
                }
                SiblingCommit::Excluded { permanent: true } => {
                    TurnDecision::Evict(EvictionReason::PermanentExclusion)
                }
                SiblingCommit::Excluded { permanent: false } | SiblingCommit::Invalid => {
                    TurnDecision::Defer(DeferReason::BlockLimits)
                }
            },
        };
        Ok(self.apply_decision(info, best_txs, group, decision, config))
    }

    /// Applies a decision that executes nothing: park, defer or evict.
    fn apply_decision<B>(
        &self,
        info: &mut ExecutionInfo,
        best_txs: &mut B,
        group: &CandidateGroup,
        decision: TurnDecision,
        config: &CandidateBuilderConfig,
    ) -> Option<()>
    where
        B: ParkablePayloadTransactions<Transaction: PoolTransaction>,
    {
        trace!(
            target: "payload_builder",
            group_hash = %group.group_hash(),
            decision = decision.as_label(),
            "candidate group turn"
        );
        match decision {
            TurnDecision::Evict(reason) => {
                config.runtime.ledger.evict(group.group_hash(), reason);
                info.permanently_rejected_txs.push(group.representative().hash());
                best_txs.mark_invalid(group.sender(), group.nonce());
            }
            // Park and defer hold the lane and never mark the nonce committed.
            _ => best_txs.park_current(),
        }
        None
    }

    /// The decision when every sibling failed admission before any trial ran.
    fn without_trials(outcomes: &[TrialOutcome], fallback: FallbackMode) -> TurnDecision {
        let permanent = |outcome: &TrialOutcome| match outcome {
            TrialOutcome::Ineligible(reason) => reason.is_permanent(),
            TrialOutcome::Eligible { .. } => false,
        };
        if outcomes.iter().all(permanent) {
            return TurnDecision::Evict(EvictionReason::PermanentExclusion);
        }
        if fallback == FallbackMode::DefaultSibling {
            // Ordinary checks on the default sibling decide between commit, defer and evict.
            return TurnDecision::Fallback(FallbackReason::NoEligibleSibling);
        }
        TurnDecision::Defer(DeferReason::BlockLimits)
    }

    fn record_permanent_exclusions(
        &self,
        group: &CandidateGroup,
        outcomes: &[TrialOutcome],
        config: &CandidateBuilderConfig,
    ) {
        for (index, outcome) in outcomes.iter().enumerate() {
            if let TrialOutcome::Ineligible(reason) = outcome
                && reason.is_permanent()
                && *reason != Ineligibility::PermanentlyExcluded
            {
                config.runtime.ledger.mark_permanently_ineligible(group.group_hash(), index);
            }
        }
    }

    fn landed(
        group: &CandidateGroup,
        index: usize,
        limits: &CandidateBlockLimits,
        config: &CandidateBuilderConfig,
    ) {
        config.runtime.ledger.record_commit(
            group.group_hash(),
            group.siblings()[index].hash(),
            limits.block_number,
        );
    }

    /// Conservative work reservation for a round over `round` siblings (design §5.6.1).
    fn round_gas(group: &CandidateGroup, round: &[usize], config: &CandidateBuilderConfig) -> u64 {
        let probes = if group.envelope().rule.token.is_zero() { 0 } else { round.len() + 1 };
        let trial_gas = round
            .iter()
            .map(|index| group.siblings()[*index].transaction.gas_limit())
            .fold(0_u64, u64::saturating_add);
        trial_gas.saturating_add(config.probe_gas.saturating_mul(probes as u64))
    }

    /// Static DA/gas limits and simulated resource admission for one sibling.
    fn sibling_precheck(
        &self,
        info: &ExecutionInfo,
        sibling: &CandidateSibling,
        limits: &CandidateBlockLimits,
    ) -> Result<(), Ineligibility> {
        if info.is_tx_over_limits(
            Self::sibling_da_size(sibling),
            limits.block_gas_limit,
            limits.tx_da_limit,
            limits.block_da_limit,
            sibling.transaction.gas_limit(),
            limits.da_footprint_gas_scalar,
        ) {
            return Err(Ineligibility::BlockLimits);
        }
        let (_, admission) = self
            .builder_config
            .resource_metering
            .check_simulated_usage(&sibling.hash(), &info.resource_metering_usage);
        if admission.should_exclude() {
            return Err(Ineligibility::SimulatedResource { permanent: admission.is_permanent() });
        }
        Ok(())
    }

    fn sibling_da_size(sibling: &CandidateSibling) -> u64 {
        BasePooledTransaction::<BaseTransactionSigned>::new(
            sibling.transaction.clone(),
            sibling.encoded.len(),
        )
        .estimated_da_size()
    }

    fn consensus_sibling(
        sibling: &CandidateSibling,
    ) -> WithEncoded<Recovered<TxTy<Evm::Primitives>>> {
        let transaction = TxTy::<Evm::Primitives>::from(sibling.transaction.inner().clone());
        WithEncoded::new(
            sibling.encoded.clone(),
            Recovered::new_unchecked(transaction, sibling.transaction.signer()),
        )
    }

    /// Runs every sibling in `round` as an uncommitted trial and scores it.
    ///
    /// Returns the work gas consumed, or `None` if the job was cancelled mid-round.
    fn evaluate_round<Builder>(
        &self,
        cumulative: &[u128],
        builder: &mut Builder,
        group: &CandidateGroup,
        round: &[usize],
        outcomes: &mut [TrialOutcome],
        config: &CandidateBuilderConfig,
    ) -> Result<Option<u64>, PayloadBuilderError>
    where
        Builder: BlockBuilder<Primitives = Evm::Primitives>,
        <<Builder::Executor as BlockExecutor>::Evm as AlloyEvm>::DB: Database,
    {
        let rule = &group.envelope().rule;
        let mut work = 0_u64;
        let evm_env = if rule.token.is_zero() { None } else { Some(self.probe_env()?) };
        let before = match &evm_env {
            None => {
                let db = builder.evm_mut().db_mut();
                Ok(db
                    .basic(rule.recipient)
                    .map_err(BlockExecutionError::other)?
                    .map_or(U256::ZERO, |account| account.balance))
            }
            Some(env) => {
                let (result, gas) = self.probe_balance(builder, env.clone(), None, config, rule)?;
                work = work.saturating_add(gas);
                result
            }
        };
        for &index in round {
            if self.cancel.is_cancelled() {
                return Ok(None);
            }
            let sibling = &group.siblings()[index];
            let trial =
                self.trial(builder, sibling, rule.recipient, evm_env.is_some(), cumulative)?;
            let gas_used = match trial.outcome {
                Ok(gas_used) => gas_used,
                Err(reason) => {
                    // Invalid siblings consume time without EVM gas; charge their declared gas.
                    work = work.saturating_add(match reason {
                        Ineligibility::Invalid => 0,
                        _ => sibling.transaction.gas_limit(),
                    });
                    outcomes[index] = TrialOutcome::Ineligible(reason);
                    continue;
                }
            };
            work = work.saturating_add(gas_used);
            let before = match before {
                Ok(before) => before,
                Err(reason) => {
                    // A failed before-probe makes every trial in the round ineligible.
                    outcomes[index] = TrialOutcome::Ineligible(reason);
                    continue;
                }
            };
            let after = match &evm_env {
                None => Ok(trial.recipient_balance.unwrap_or(before)),
                Some(env) => {
                    let (result, gas) = self.probe_balance(
                        builder,
                        env.clone(),
                        trial.state.as_ref(),
                        config,
                        rule,
                    )?;
                    work = work.saturating_add(gas);
                    result
                }
            };
            outcomes[index] =
                match after.and_then(|after| ScoreRead::delta(before, after, rule.min_delta)) {
                    Ok(delta) => TrialOutcome::Eligible { delta, gas_used },
                    Err(reason) => TrialOutcome::Ineligible(reason),
                };
        }
        Ok(Some(work))
    }

    /// Executes one sibling without committing and applies executed resource admission.
    fn trial<Builder>(
        &self,
        builder: &mut Builder,
        sibling: &CandidateSibling,
        recipient: Address,
        keep_state: bool,
        cumulative: &[u128],
    ) -> Result<SiblingTrial, PayloadBuilderError>
    where
        Builder: BlockBuilder<Primitives = Evm::Primitives>,
    {
        let resource_metering = &self.builder_config.resource_metering;
        let hash = sibling.hash();
        let (simulated, _) = resource_metering.check_simulated_usage(&hash, cumulative);
        let mut captured = None;
        let executed = builder.execute_transaction_with_commit_condition(
            Self::consensus_sibling(sibling),
            |result| {
                let result_and_state = result.result();
                let gas_used = result_and_state.result.tx_gas_used();
                let decision = resource_metering.check_executed_usage(
                    &hash,
                    gas_used,
                    &result_and_state.state,
                    simulated.as_ref(),
                    cumulative,
                );
                let outcome = if decision.should_exclude() {
                    Err(Ineligibility::ExecutedResource { permanent: decision.is_permanent() })
                } else if result_and_state.result.is_halt() {
                    Err(Ineligibility::Halted)
                } else if !result_and_state.result.is_success() {
                    Err(Ineligibility::Reverted)
                } else {
                    Ok(gas_used)
                };
                let recipient_balance = result_and_state
                    .state
                    .get(&recipient)
                    .filter(|account| account.is_touched())
                    .map(|account| {
                        if account.is_selfdestructed() { U256::ZERO } else { account.info.balance }
                    });
                captured = Some(SiblingTrial {
                    outcome,
                    state: keep_state.then(|| result_and_state.state.clone()),
                    recipient_balance,
                });
                // Trials never change the committed prefix (design §5.3).
                CommitChanges::No
            },
        );
        match executed {
            Ok(_) => Ok(captured.unwrap_or_else(SiblingTrial::invalid)),
            Err(BlockExecutionError::Validation(BlockValidationError::InvalidTx { .. })) => {
                Ok(SiblingTrial::invalid())
            }
            Err(error) => Err(PayloadBuilderError::EvmExecutionError(Box::new(error))),
        }
    }

    /// Returns the EVM environment of the block being built, for score probes.
    fn probe_env(&self) -> Result<EvmEnvFor<Evm>, PayloadBuilderError> {
        let next_env = Evm::NextBlockEnvCtx::build_next_env(
            self.attributes(),
            self.parent(),
            self.chain_spec.as_ref(),
        )
        .map_err(PayloadBuilderError::other)?;
        self.evm_config.next_evm_env(self.parent(), &next_env).map_err(PayloadBuilderError::other)
    }

    /// Reads `rule.recipient`'s token balance on a discarded overlay (score-read context v1).
    ///
    /// The probe is a gas-capped, zero-value, fee-free call from `address(0)`. Returns the
    /// decoded balance or the trial ineligibility, plus the probe gas used.
    fn probe_balance<Builder>(
        &self,
        builder: &mut Builder,
        evm_env: EvmEnvFor<Evm>,
        diff: Option<&EvmState>,
        config: &CandidateBuilderConfig,
        rule: &CandidateSelectionRule,
    ) -> Result<(Result<U256, Ineligibility>, u64), PayloadBuilderError>
    where
        Builder: BlockBuilder<Primitives = Evm::Primitives>,
        <<Builder::Executor as BlockExecutor>::Evm as AlloyEvm>::DB: Database,
    {
        let probe = TxDeposit {
            // Probes run on a discarded overlay and are never included in a block.
            source_hash: B256::ZERO,
            from: Address::ZERO,
            to: TxKind::Call(rule.token),
            mint: 0,
            value: U256::ZERO,
            gas_limit: config.probe_gas,
            is_system_transaction: false,
            input: ScoreRead::balance_of_calldata(rule.recipient),
        };
        let probe = Recovered::new_unchecked(TxTy::<Evm::Primitives>::from(probe), Address::ZERO);
        let overlay = ScoreOverlay::new(builder.evm_mut().db_mut(), diff);
        let mut evm = self.evm_config.evm_with_env(overlay, evm_env);
        let result = match evm.transact(&probe) {
            Ok(ResultAndState { result, .. }) => result,
            Err(error) if EvmError::is_invalid_tx_err(&error) => {
                return Ok((Err(Ineligibility::ScoreHalted), 0));
            }
            Err(error) => return Err(PayloadBuilderError::other(error)),
        };
        let gas_used = result.tx_gas_used();
        let probe_result = match result {
            ExecutionResult::Success { output, .. } => ProbeResult::Success(output.into_data()),
            ExecutionResult::Revert { .. } => ProbeResult::Revert,
            ExecutionResult::Halt { .. } => ProbeResult::Halt,
        };
        Ok((ScoreRead::decode_balance(&probe_result), gas_used))
    }

    /// Returns `true` when the group's shared nonce is below the sender's prefix nonce.
    fn nonce_already_used<Builder>(&self, builder: &mut Builder, group: &CandidateGroup) -> bool
    where
        Builder: BlockBuilder<Primitives = Evm::Primitives>,
        <<Builder::Executor as BlockExecutor>::Evm as AlloyEvm>::DB: Database,
    {
        builder
            .evm_mut()
            .db_mut()
            .basic(group.sender())
            .ok()
            .flatten()
            .is_some_and(|account| account.nonce > group.nonce())
    }

    /// Commits one sibling through the ordinary resource-checked path and accounts it.
    fn commit_sibling<Builder>(
        &self,
        info: &mut ExecutionInfo,
        builder: &mut Builder,
        group: &CandidateGroup,
        index: usize,
        limits: &CandidateBlockLimits,
    ) -> Result<SiblingCommit, PayloadBuilderError>
    where
        Builder: BlockBuilder<Primitives = Evm::Primitives>,
    {
        let sibling = &group.siblings()[index];
        let hash = sibling.hash();
        let resource_metering = &self.builder_config.resource_metering;
        let (simulated, _) =
            resource_metering.check_simulated_usage(&hash, &info.resource_metering_usage);
        let mut pending_resource_usage = None;
        let mut executed_decision = None;
        let tx = TxTy::<Evm::Primitives>::from(sibling.transaction.inner().clone());
        let consensus = WithEncoded::new(
            sibling.encoded.clone(),
            Recovered::new_unchecked(tx.clone(), sibling.transaction.signer()),
        );
        let gas_output =
            match builder.execute_transaction_with_commit_condition(consensus, |result| {
                let result_and_state = result.result();
                let decision = resource_metering.check_executed_usage(
                    &hash,
                    result_and_state.result.tx_gas_used(),
                    &result_and_state.state,
                    simulated.as_ref(),
                    &info.resource_metering_usage,
                );
                if decision.should_exclude() {
                    executed_decision = Some(decision);
                    CommitChanges::No
                } else {
                    pending_resource_usage = decision.committed_usage();
                    CommitChanges::Yes
                }
            }) {
                Ok(Some(gas_output)) => gas_output,
                Ok(None) => {
                    let permanent =
                        executed_decision.as_ref().is_some_and(|decision| decision.is_permanent());
                    if permanent {
                        info.permanently_rejected_txs.push(hash);
                    }
                    return Ok(SiblingCommit::Excluded { permanent });
                }
                Err(BlockExecutionError::Validation(BlockValidationError::InvalidTx {
                    error,
                    ..
                })) => {
                    return Ok(if error.is_nonce_too_low() {
                        SiblingCommit::NonceTooLow
                    } else {
                        SiblingCommit::Invalid
                    });
                }
                Err(error) => return Err(PayloadBuilderError::EvmExecutionError(Box::new(error))),
            };

        let gas_used = gas_output.tx_gas_used();
        info.cumulative_gas_used += gas_used;
        info.cumulative_da_bytes_used += Self::sibling_da_size(sibling);
        if let Some(usage) = pending_resource_usage {
            resource_metering.apply_accounted_usage(
                &hash,
                &usage,
                &mut info.resource_metering_usage,
            );
        }
        let miner_fee = tx
            .effective_tip_per_gas(limits.base_fee)
            .expect("fee is always valid; execution succeeded");
        info.total_fees += U256::from(miner_fee) * U256::from(gas_used);
        info.inclusion.record(false, gas_used, miner_fee, limits.base_fee, U256::ZERO);
        BuilderMetrics::record_tip_per_gas(false, false, miner_fee as f64);
        Ok(SiblingCommit::Committed)
    }
}

#[cfg(test)]
mod tests {}
