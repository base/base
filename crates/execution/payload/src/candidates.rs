//! Builder-side candidate-group selection for the native payload builder (design-v2 §5).
//!
//! When the pool iterator yields a representative carrying a [`CandidateGroup`], the builder
//! applies the normative lifecycle precedence ([`GroupLifecycle`]), runs every eligible sibling
//! as an uncommitted trial at the frozen prefix, scores each trial with score-read context v1,
//! and commits at most one sibling (the winner, or the signed default sibling as fallback)
//! through the ordinary resource-checked commit path. Park, defer and evict never mark the
//! nonce committed.

use std::{
    collections::BTreeMap,
    sync::Arc,
    time::{Duration, Instant},
};

use alloy_consensus::{BlockHeader, Transaction};
use alloy_evm::{
    Evm as AlloyEvm, EvmError,
    block::{CommitChanges, TxResult},
};
use alloy_primitives::{Address, B256, TxHash, TxKind, U256};
use base_common_chains::Upgrades;
use base_common_consensus::{BaseTransactionSigned, TxDeposit};
use base_execution_candidates::{
    AccessDenied, AttemptId, CandidateGroup, CandidateRuntime, CandidateSelectionRule,
    CandidateSibling, DEFAULT_MAX_EVALUATIONS, DEFAULT_PROBE_GAS, DeferReason, EvictionReason,
    FallbackMode, FallbackReason, GroupLifecycle, Ineligibility, ParkReason, ProbeResult,
    ReserveError, ScoreRead, TrialOutcome, TurnContext, TurnDecision, WinnerSelection,
};
use base_execution_txpool::{BasePooledTransaction, estimated_da_size::DataAvailabilitySized};
use base_observability_events::{
    GlobalTransactionEventWriter, TransactionEventProducer, TransactionEventType, transaction_event,
};
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
    Database as _, Inspector,
    context::result::{ExecutionResult, ResultAndState},
    interpreter::{CallInputs, CallOutcome},
    state::{Account, AccountInfo, Bytecode, EvmState},
};
use tracing::{debug, trace, warn};

use crate::{
    Attributes, BuilderMetrics, ParkablePayloadTransactions, PayloadPrimitives,
    builder::{BasePayloadBuilderCtx, ExecutionInfo},
};

/// Journals a candidate-group transaction event for the payload being built, when the turn's
/// [`CandidateJournal`] is enabled. The event ID binds the ledger instance, the turn and the
/// group, plus any extra `[id]` parts, so distinct turns never deduplicate while a redelivered
/// event does. Data carries `group_hash` and `representative_hash` so a group's events join
/// without the sibling list.
macro_rules! emit_candidate_event {
    ($ctx:expr, $journal:expr, $event_type:expr, $tx_hash:expr, $group:expr,
        [ $( $id_name:expr => $id_value:expr ),* $(,)? ],
        { $( $data_name:expr => $data_value:expr ),* $(,)? }
    ) => {
        if $journal.enabled && GlobalTransactionEventWriter::get().is_some() {
            let tx_hash: TxHash = $tx_hash;
            let instance = format!("{:016x}", $journal.instance);
            let group_hash = format!("{:#x}", $group.group_hash());
            if let Err(error) = transaction_event!(
                producer: TransactionEventProducer::BaseBuilder,
                event_type: $event_type,
                tx_hash: tx_hash,
                block_number: $ctx.parent().number().saturating_add(1),
                payload_id: $ctx.payload_id().to_string(),
                id: {
                    "candidate_instance" => &instance,
                    "candidate_turn" => $journal.turn,
                    "group_hash" => &group_hash,
                    $( $id_name => $id_value ),*
                },
                data: {
                    "builder_mode" => "native",
                    "source_queue" => "candidate_group",
                    "parent_hash" => format!("{:#x}", $ctx.parent().hash()),
                    "group_hash" => group_hash,
                    "representative_hash" => format!("{:#x}", $group.representative().hash()),
                    "candidate_instance" => instance,
                    "turn" => $journal.turn,
                    $( $data_name => $data_value ),*
                },
            ) {
                warn!(
                    target: "payload_builder",
                    error = %error,
                    event_type = %$event_type,
                    tx_hash = %tx_hash,
                    "failed to enqueue candidate group transaction event"
                );
            }
        }
    };
}

/// Default per-build wall-time budget for candidate handling.
pub const DEFAULT_CANDIDATE_EVAL_WALL_CUTOFF: Duration = Duration::from_millis(10);

/// Default time after a build starts its pool scan past which no candidate work starts.
///
/// Placeholder below the 200 ms native block cadence until Phase 0 measurements choose it
/// (design Q5).
pub const DEFAULT_CANDIDATE_BUILD_DEADLINE: Duration = Duration::from_millis(100);

/// Default work gas all candidate rounds of one build may reserve: two 30M-gas blocks of
/// trials. Placeholder until Phase 0 measurements choose it (design Q5).
pub const DEFAULT_CANDIDATE_BUILD_WORK_GAS: u64 = 60_000_000;

/// Default work charged for a sibling that fails pre-EVM validation (design §5.6.2
/// `invalid_attempt_gas`): the intrinsic cost of the cheapest transaction.
pub const DEFAULT_INVALID_ATTEMPT_GAS: u64 = 21_000;

/// Candidate selection settings for the native builder.
#[derive(Debug, Clone)]
pub struct CandidateBuilderConfig {
    /// Shared ledger, policy and kill switch.
    pub runtime: Arc<CandidateRuntime>,
    /// Gas cap for each score-read probe (`G_probe`).
    pub probe_gas: u64,
    /// Wall time candidate handling may spend per payload build.
    pub eval_wall_cutoff: Duration,
    /// Time after the pool scan starts past which no candidate trial, probe or commit starts.
    pub build_deadline: Duration,
    /// Work gas all candidate rounds of one build may reserve.
    pub build_work_gas: u64,
    /// Work charged for a sibling that fails pre-EVM validation.
    pub invalid_attempt_gas: u64,
    /// Server cap on evaluations per group, applied below the signed `maxEvaluations`.
    pub server_max_evaluations: u16,
    /// Whether each sibling outcome gets a `BUILDER_CANDIDATE_TRIAL` event.
    pub trial_events: bool,
}

impl CandidateBuilderConfig {
    /// Creates a configuration with default probe gas, wall cutoff and evaluation cap.
    pub const fn new(runtime: Arc<CandidateRuntime>) -> Self {
        Self {
            runtime,
            probe_gas: DEFAULT_PROBE_GAS,
            eval_wall_cutoff: DEFAULT_CANDIDATE_EVAL_WALL_CUTOFF,
            build_deadline: DEFAULT_CANDIDATE_BUILD_DEADLINE,
            build_work_gas: DEFAULT_CANDIDATE_BUILD_WORK_GAS,
            invalid_attempt_gas: DEFAULT_INVALID_ATTEMPT_GAS,
            server_max_evaluations: DEFAULT_MAX_EVALUATIONS,
            trial_events: false,
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
    /// Absolute time past which no candidate trial, probe or commit starts.
    pub deadline: Option<Instant>,
    /// Work gas reserved or settled by candidate rounds in this build.
    pub work_gas: u64,
    /// When the running group turn started.
    pub turn_started: Option<Instant>,
    /// Whether the builder has parked validity predicates that a commit may wake.
    pub track_committed_state: bool,
    /// State committed by the latest candidate sibling, when tracked, for the builder's
    /// predicate wakeups.
    pub committed_state: Option<EvmState>,
}

impl CandidateScan {
    /// Starts a scan at `started`, applying the configured build deadline.
    pub fn new(config: Option<&CandidateBuilderConfig>, started: Instant) -> Self {
        Self { deadline: config.map(|config| started + config.build_deadline), ..Self::default() }
    }

    /// Returns `true` while the build deadline and the per-build wall cutoff, including the
    /// running turn, both leave time for more candidate work.
    pub fn has_time(&self, config: &CandidateBuilderConfig) -> bool {
        let turn = self.turn_started.map_or(Duration::ZERO, |started| started.elapsed());
        self.eval_duration + turn < config.eval_wall_cutoff
            && self.deadline.is_none_or(|deadline| Instant::now() < deadline)
    }
}

/// How an evaluation round ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoundEnd {
    /// Every sibling in the round ran.
    Completed,
    /// The payload job was cancelled.
    Cancelled,
    /// The build deadline or wall cutoff stopped the round.
    OutOfTime,
    /// The payload job asked to finalize.
    Finalized,
}

impl RoundEnd {
    /// Returns a stable label for logs.
    pub const fn as_label(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Cancelled => "cancelled",
            Self::OutOfTime => "out_of_time",
            Self::Finalized => "finalized",
        }
    }

    /// The deferral for a round that stopped early; `None` when it completed or was cancelled.
    pub const fn defer_reason(self) -> Option<DeferReason> {
        match self {
            Self::Completed | Self::Cancelled => None,
            Self::OutOfTime => Some(DeferReason::Deadline),
            Self::Finalized => Some(DeferReason::Finalization),
        }
    }
}

/// One evaluation round's siblings, outcomes and settled work.
#[derive(Debug)]
pub struct CandidateRound {
    /// Sibling indexes that passed admission and run in this round.
    pub siblings: Vec<usize>,
    /// Outcome per sibling, in envelope order.
    pub outcomes: Vec<TrialOutcome>,
    /// Work gas actually consumed so far.
    pub work: u64,
}

/// Makes the probe's top-level frame a `STATICCALL` (score-read context v1, design §3.4): any
/// state change in the token, or in contracts it calls, halts the probe.
#[derive(Debug, Default, Clone, Copy)]
pub struct StaticTopLevelCall {
    /// Whether the top-level frame has started.
    pub started: bool,
}

impl<CTX> Inspector<CTX> for StaticTopLevelCall {
    fn call(&mut self, _context: &mut CTX, inputs: &mut CallInputs) -> Option<CallOutcome> {
        if !self.started {
            self.started = true;
            inputs.is_static = true;
        }
        None
    }
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

    /// Returns the trial diff's entry for `address` when the trial touched it.
    pub fn touched(&self, address: &Address) -> Option<&'a Account> {
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
    /// Event identity and switches for this turn.
    pub journal: CandidateJournal,
}

/// Event identity and switches for one candidate turn.
///
/// Round and decision events are journaled whenever a writer is configured and selection is
/// enabled; a runtime-disabled builder journals nothing, like one started without candidates.
/// Per-sibling trial events also need [`CandidateBuilderConfig::trial_events`].
#[derive(Debug, Clone, Copy)]
pub struct CandidateJournal {
    /// Whether round and decision events are journaled.
    pub enabled: bool,
    /// Whether trial events are journaled; implies `enabled`.
    pub trials: bool,
    /// [`GroupLedger::instance`](base_execution_candidates::GroupLedger::instance), which
    /// distinguishes processes.
    pub instance: u64,
    /// This turn's never-reused identity.
    pub turn: u64,
}

impl CandidateJournal {
    /// Allocates a turn identity and reads the switches for one turn.
    pub fn new(config: &CandidateBuilderConfig) -> Self {
        let ledger = &config.runtime.ledger;
        let enabled =
            config.runtime.switch.is_enabled() && GlobalTransactionEventWriter::get().is_some();
        Self {
            enabled,
            trials: enabled && config.trial_events,
            instance: ledger.instance(),
            turn: ledger.next_sequence(),
        }
    }
}

/// Which sibling outcomes [`BasePayloadBuilderCtx::journal_trials`] reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrialStage {
    /// Siblings that failed admission before any trial.
    Precheck,
    /// Siblings that ran in the round.
    Trial,
}

impl TrialStage {
    /// Returns the stable `stage` label.
    pub const fn as_label(self) -> &'static str {
        match self {
            Self::Precheck => "precheck",
            Self::Trial => "trial",
        }
    }
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
    /// The simulated sample the schedule requires is no longer available.
    Unmetered,
}

/// A sibling's uncommitted trial.
#[derive(Debug)]
pub struct SiblingTrial {
    /// Gas used, or why the trial is ineligible before scoring.
    pub outcome: Result<u64, Ineligibility>,
    /// Gas the EVM consumed, including for reverted, halted and resource-excluded trials.
    pub gas_used: u64,
    /// The trial's state diff, kept only when a token probe needs it.
    pub state: Option<EvmState>,
    /// The recipient's post-trial native balance, when the trial touched it.
    pub recipient_balance: Option<U256>,
}

impl SiblingTrial {
    /// A trial that failed pre-EVM validation.
    pub const fn invalid() -> Self {
        Self {
            outcome: Err(Ineligibility::Invalid),
            gas_used: 0,
            state: None,
            recipient_balance: None,
        }
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
        scan.turn_started = Some(started);
        let turn = CandidateTurn { group, limits, config, journal: CandidateJournal::new(config) };
        let result = self.candidate_turn(info, builder, best_txs, turn, scan);
        scan.turn_started = None;
        scan.eval_duration += started.elapsed();
        result
    }

    /// Runs the lifecycle, admission, reservation, evaluation and commit of one group turn.
    pub fn candidate_turn<Builder, B>(
        &self,
        info: &mut ExecutionInfo,
        builder: &mut Builder,
        best_txs: &mut B,
        turn: CandidateTurn<'_>,
        scan: &mut CandidateScan,
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
            build_budget_available: scan.has_time(config),
        });
        let Some(entry) = entry else {
            return Ok(self.apply_decision(info, best_txs, turn, decision));
        };
        if decision != TurnDecision::Evaluate {
            return self.decide(info, builder, best_txs, turn, decision, scan);
        }

        // Per-sibling static and simulated admission at the unchanged prefix; nothing reserved.
        let mut round = CandidateRound {
            siblings: Vec::new(),
            outcomes: Vec::with_capacity(group.siblings().len()),
            work: 0,
        };
        for (index, sibling) in group.siblings().iter().enumerate() {
            let check = if entry.permanently_ineligible.get(index).copied().unwrap_or(false) {
                Err(Ineligibility::PermanentlyExcluded)
            } else {
                self.sibling_precheck(info, sibling, limits)
            };
            match check {
                Ok(()) => {
                    round.siblings.push(index);
                    round.outcomes.push(TrialOutcome::Ineligible(Ineligibility::NotRun));
                }
                Err(reason) => round.outcomes.push(TrialOutcome::Ineligible(reason)),
            }
        }
        Self::record_permanent_exclusions(group, &round.outcomes, config);
        self.journal_trials(turn, &round, TrialStage::Precheck);
        if round.siblings.is_empty() {
            let decision = Self::without_trials(&round.outcomes, group.fallback());
            return self.decide(info, builder, best_txs, turn, decision, scan);
        }

        // Reserve build work, then principal work, so neither failure consumes an attempt.
        let round_gas = Self::round_gas(group, &round.siblings, config);
        if scan.work_gas.saturating_add(round_gas) > config.build_work_gas {
            let decision = TurnDecision::Defer(DeferReason::BuildBudget);
            return Ok(self.apply_decision(info, best_txs, turn, decision));
        }
        let attempt = AttemptId {
            group_hash,
            parent_hash: self.parent().hash(),
            seq: entry.attempts.saturating_add(1),
            turn: turn.journal.turn,
        };
        let ticket = match runtime.policy.reserve(entry.principal, attempt, round_gas) {
            Ok(ticket) => ticket,
            Err(AccessDenied::NotAllowed) => {
                let decision = TurnDecision::Evict(EvictionReason::Revoked);
                return Ok(self.apply_decision(info, best_txs, turn, decision));
            }
            Err(_) => {
                let decision = TurnDecision::Defer(DeferReason::PrincipalQuota);
                return Ok(self.apply_decision(info, best_txs, turn, decision));
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
                    return self.decide(info, builder, best_txs, turn, decision, scan);
                }
            };

        // The build budget holds the full reservation while the round runs, as the ledger and
        // policy do, so a concurrent round could not oversubscribe it. Every round settles at
        // the work it actually consumed, whether it completed, ran out of time, was cancelled or
        // failed (design §5.6.2).
        scan.work_gas = scan.work_gas.saturating_add(round_gas);
        let evaluation =
            self.evaluate_round(&info.resource_metering_usage, builder, turn, &mut round, scan);
        let work = round.work.min(round_gas);
        let attempt_seq = reservation.attempt;
        runtime.ledger.settle(reservation, work);
        runtime.policy.settle(ticket, work);
        scan.work_gas = scan.work_gas.saturating_sub(round_gas).saturating_add(work);
        let end_label = evaluation.as_ref().map_or("error", |end| end.as_label());
        self.journal_trials(turn, &round, TrialStage::Trial);
        self.journal_round(turn, &round, attempt_seq, round_gas, end_label);
        debug!(
            target: "payload_builder",
            group_hash = %group_hash,
            attempt = attempt.seq,
            reserved_work_gas = round_gas,
            settled_work_gas = work,
            outcome = end_label,
            "settled candidate round"
        );
        let end = evaluation?;
        if end != RoundEnd::Completed {
            return Ok(self.interrupt(info, best_txs, turn, end, attempt_seq));
        }
        let outcomes = round.outcomes;
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
            self.journal_decision(turn, TurnDecision::Evict(EvictionReason::Dropped));
            best_txs.mark_current_committed();
            return Ok(None);
        }
        Self::record_permanent_exclusions(group, &outcomes, config);

        let winner = WinnerSelection::choose(&outcomes);
        let decision = match winner {
            Some(_) => TurnDecision::CommitWinner,
            // A sample that disappeared since precheck fails closed (design §5.2).
            None if outcomes.contains(&TrialOutcome::Ineligible(Ineligibility::Unmetered)) => {
                TurnDecision::Defer(DeferReason::Unmetered)
            }
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
        // A stop that arrived during the last trial still prevents the winner or fallback from
        // executing; the round defers without consuming its attempt.
        if matches!(decision, TurnDecision::CommitWinner | TurnDecision::Fallback(_))
            && let Some(end) = self.round_stop(scan, config)
        {
            return Ok(self.interrupt(info, best_txs, turn, end, attempt_seq));
        }
        match (decision, winner) {
            (TurnDecision::CommitWinner, Some(index)) => {
                match self.commit_sibling(info, builder, turn, index, scan)? {
                    SiblingCommit::Committed => {
                        self.landed(turn, index, None);
                        best_txs.mark_current_committed();
                    }
                    SiblingCommit::NonceTooLow => {
                        runtime.ledger.evict(group_hash, EvictionReason::Dropped);
                        self.journal_decision(turn, TurnDecision::Evict(EvictionReason::Dropped));
                        best_txs.mark_current_committed();
                    }
                    SiblingCommit::Unmetered => {
                        let decision = TurnDecision::Defer(DeferReason::Unmetered);
                        self.apply_decision(info, best_txs, turn, decision);
                    }
                    // The live re-execution disagreed with the trial at the same prefix.
                    SiblingCommit::Excluded { .. } | SiblingCommit::Invalid => {
                        let decision = TurnDecision::Defer(DeferReason::VerifyMismatch);
                        self.apply_decision(info, best_txs, turn, decision);
                    }
                }
                Ok(None)
            }
            (decision, _) => self.decide(info, builder, best_txs, turn, decision, scan),
        }
    }

    /// Applies a decision that may run the fallback sibling.
    pub fn decide<Builder, B>(
        &self,
        info: &mut ExecutionInfo,
        builder: &mut Builder,
        best_txs: &mut B,
        turn: CandidateTurn<'_>,
        decision: TurnDecision,
        scan: &mut CandidateScan,
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
            return Ok(self.apply_decision(info, best_txs, turn, decision));
        };
        let index = usize::from(group.envelope().default_index);
        let decision = if let Some(end) = self.round_stop(scan, config) {
            match end.defer_reason() {
                None => return Ok(Some(())),
                Some(reason) => TurnDecision::Defer(reason),
            }
        } else {
            match self.sibling_precheck(info, &group.siblings()[index], limits) {
                Err(reason) if reason.is_permanent() => {
                    TurnDecision::Evict(EvictionReason::PermanentExclusion)
                }
                Err(Ineligibility::Unmetered) => TurnDecision::Defer(DeferReason::Unmetered),
                Err(_) => TurnDecision::Defer(DeferReason::BlockLimits),
                Ok(()) => match self.commit_sibling(info, builder, turn, index, scan)? {
                    SiblingCommit::Committed => {
                        debug!(
                            target: "payload_builder",
                            group_hash = %group.group_hash(),
                            reason = ?reason,
                            "committed candidate fallback sibling"
                        );
                        self.landed(turn, index, Some(reason));
                        best_txs.mark_current_committed();
                        return Ok(None);
                    }
                    SiblingCommit::NonceTooLow => {
                        config.runtime.ledger.evict(group.group_hash(), EvictionReason::Dropped);
                        self.journal_decision(turn, TurnDecision::Evict(EvictionReason::Dropped));
                        best_txs.mark_current_committed();
                        return Ok(None);
                    }
                    SiblingCommit::Excluded { permanent: true } => {
                        TurnDecision::Evict(EvictionReason::PermanentExclusion)
                    }
                    SiblingCommit::Unmetered => TurnDecision::Defer(DeferReason::Unmetered),
                    SiblingCommit::Excluded { permanent: false } | SiblingCommit::Invalid => {
                        TurnDecision::Defer(DeferReason::BlockLimits)
                    }
                },
            }
        };
        Ok(self.apply_decision(info, best_txs, turn, decision))
    }

    /// Ends a turn whose round was interrupted by cancellation, finalization or the deadline.
    ///
    /// The round's settled work stays charged but its attempt is returned (design §5.7 step 7).
    /// Cancellation stops the build; the other stops defer the group.
    pub fn interrupt<B>(
        &self,
        info: &mut ExecutionInfo,
        best_txs: &mut B,
        turn: CandidateTurn<'_>,
        end: RoundEnd,
        attempt: u16,
    ) -> Option<()>
    where
        B: ParkablePayloadTransactions<Transaction: PoolTransaction>,
    {
        turn.config.runtime.ledger.release_attempt(turn.group.group_hash(), attempt);
        end.defer_reason().map_or(Some(()), |reason| {
            let decision = TurnDecision::Defer(reason);
            self.apply_decision(info, best_txs, turn, decision)
        })
    }

    /// Applies a decision that executes nothing: park, defer or evict.
    ///
    /// Eviction removes the representative from the pool through
    /// [`ExecutionInfo::candidate_evictions`]. It never enters the rejection cache: the
    /// representative's raw hash is only the group's lifecycle identity.
    pub fn apply_decision<B>(
        &self,
        info: &mut ExecutionInfo,
        best_txs: &mut B,
        turn: CandidateTurn<'_>,
        decision: TurnDecision,
    ) -> Option<()>
    where
        B: ParkablePayloadTransactions<Transaction: PoolTransaction>,
    {
        let group = turn.group;
        trace!(
            target: "payload_builder",
            group_hash = %group.group_hash(),
            decision = decision.as_label(),
            "candidate group turn"
        );
        self.journal_decision(turn, decision);
        match decision {
            TurnDecision::Evict(reason) => {
                turn.config.runtime.ledger.evict(group.group_hash(), reason);
                info.candidate_evictions.push(group.representative().hash());
                best_txs.mark_invalid(group.sender(), group.nonce());
            }
            // Park and defer hold the lane and never mark the nonce committed.
            _ => best_txs.park_current(),
        }
        None
    }

    /// The decision when every sibling failed admission before any trial ran.
    pub fn without_trials(outcomes: &[TrialOutcome], fallback: FallbackMode) -> TurnDecision {
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
        if outcomes.contains(&TrialOutcome::Ineligible(Ineligibility::Unmetered)) {
            return TurnDecision::Defer(DeferReason::Unmetered);
        }
        TurnDecision::Defer(DeferReason::BlockLimits)
    }

    /// Records newly observed transaction-scope permanent exclusions in the ledger.
    pub fn record_permanent_exclusions(
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

    /// Records the builder's provisional commit of sibling `index` and journals
    /// `BUILDER_ACCEPTED` for it: `winner`, or `fallback` with the reason fallback ran. The
    /// commit is per payload, not terminal; canonical landing is journaled by the ledger.
    pub fn landed(&self, turn: CandidateTurn<'_>, index: usize, fallback: Option<FallbackReason>) {
        let group = turn.group;
        let sibling = group.siblings()[index].hash();
        turn.config.runtime.ledger.record_commit(
            group.group_hash(),
            sibling,
            turn.limits.block_number,
        );
        emit_candidate_event!(
            self,
            turn.journal,
            TransactionEventType::BuilderAccepted,
            sibling,
            group,
            [],
            {
                "candidate_selection" => if fallback.is_some() { "fallback" } else { "winner" },
                "fallback_reason" => fallback.map(FallbackReason::as_label),
                "sibling_index" => index,
            }
        );
    }

    /// Journals a park, defer or evict decision against the group's representative.
    ///
    /// Expiry is `BUILDER_EXPIRED`, other evictions are `BUILDER_REJECTED`, and park and defer
    /// are `BUILDER_DEFERRED`; the reason is [`TurnDecision::as_label`]. Decisions that execute
    /// a sibling journal through [`Self::landed`] instead. Like `BUILDER_ACCEPTED`, these are
    /// per-payload observations; the ledger journals the group's terminal event.
    pub fn journal_decision(&self, turn: CandidateTurn<'_>, decision: TurnDecision) {
        let group = turn.group;
        let representative = group.representative().hash();
        let label = decision.as_label();
        match decision {
            TurnDecision::Evict(EvictionReason::Expired) => {
                emit_candidate_event!(
                    self,
                    turn.journal,
                    TransactionEventType::BuilderExpired,
                    representative,
                    group,
                    [],
                    { "expire_reason" => label }
                );
            }
            TurnDecision::Evict(_) => {
                emit_candidate_event!(
                    self,
                    turn.journal,
                    TransactionEventType::BuilderRejected,
                    representative,
                    group,
                    [],
                    { "rejection_reason" => label, "permanent" => true }
                );
            }
            TurnDecision::Park(_) | TurnDecision::Defer(_) => {
                emit_candidate_event!(
                    self,
                    turn.journal,
                    TransactionEventType::BuilderDeferred,
                    representative,
                    group,
                    [],
                    { "defer_reason" => label }
                );
            }
            TurnDecision::Fallback(_) | TurnDecision::Evaluate | TurnDecision::CommitWinner => {}
        }
    }

    /// Journals `BUILDER_CANDIDATE_ROUND` once a reserved round settles, however it ended.
    ///
    /// `end` is the [`RoundEnd`] label or `error`. `sibling_outcomes` holds each sibling's
    /// outcome label (`eligible` or its ineligible reason) in envelope order and `outcomes`
    /// counts them, so the round stays attributable without trial events. Scores stay out of
    /// the always-on event. `winner_index` is set when a completed round has a winner.
    pub fn journal_round(
        &self,
        turn: CandidateTurn<'_>,
        round: &CandidateRound,
        attempt: u16,
        reserved_work_gas: u64,
        end: &'static str,
    ) {
        if !turn.journal.enabled {
            return;
        }
        let sibling_outcomes: Vec<&'static str> = round
            .outcomes
            .iter()
            .map(|outcome| match outcome {
                TrialOutcome::Eligible { .. } => "eligible",
                TrialOutcome::Ineligible(reason) => reason.as_label(),
            })
            .collect();
        let mut outcomes = BTreeMap::<&'static str, usize>::new();
        for label in &sibling_outcomes {
            *outcomes.entry(label).or_default() += 1;
        }
        let winner = (end == RoundEnd::Completed.as_label())
            .then(|| WinnerSelection::choose(&round.outcomes))
            .flatten();
        emit_candidate_event!(
            self,
            turn.journal,
            TransactionEventType::BuilderCandidateRound,
            turn.group.representative().hash(),
            turn.group,
            [],
            {
                "attempt" => attempt,
                "round_siblings" => round.siblings.len(),
                "reserved_work_gas" => reserved_work_gas,
                "settled_work_gas" => round.work.min(reserved_work_gas),
                "round_end" => end,
                "sibling_outcomes" => sibling_outcomes,
                "outcomes" => outcomes,
                "winner_index" => winner,
            }
        );
    }

    /// Journals `BUILDER_CANDIDATE_TRIAL` for the siblings `stage` selects, in envelope order,
    /// when trial events are enabled. Siblings a stopped round never dispatched are skipped; one
    /// that executed but was not scored is `interrupted`.
    pub fn journal_trials(
        &self,
        turn: CandidateTurn<'_>,
        round: &CandidateRound,
        stage: TrialStage,
    ) {
        if !turn.journal.trials {
            return;
        }
        let group = turn.group;
        for (index, outcome) in round.outcomes.iter().enumerate() {
            let in_round = round.siblings.contains(&index);
            if in_round != (stage == TrialStage::Trial) {
                continue;
            }
            let sibling = group.siblings()[index].hash();
            match outcome {
                TrialOutcome::Ineligible(Ineligibility::NotRun) => {}
                TrialOutcome::Eligible { delta, gas_used } => {
                    emit_candidate_event!(
                        self,
                        turn.journal,
                        TransactionEventType::BuilderCandidateTrial,
                        sibling,
                        group,
                        ["sibling_index" => index],
                        {
                            "sibling_index" => index,
                            "stage" => stage.as_label(),
                            "outcome" => "eligible",
                            "delta" => delta.to_string(),
                            "gas_used" => gas_used,
                        }
                    );
                }
                TrialOutcome::Ineligible(reason) => {
                    emit_candidate_event!(
                        self,
                        turn.journal,
                        TransactionEventType::BuilderCandidateTrial,
                        sibling,
                        group,
                        ["sibling_index" => index],
                        {
                            "sibling_index" => index,
                            "stage" => stage.as_label(),
                            "outcome" => "ineligible",
                            "ineligible_reason" => reason.as_label(),
                        }
                    );
                }
            }
        }
    }

    /// Conservative work reservation for a round over `round` siblings (design §5.6.1).
    pub fn round_gas(
        group: &CandidateGroup,
        round: &[usize],
        config: &CandidateBuilderConfig,
    ) -> u64 {
        let probes = if group.envelope().rule.token.is_zero() { 0 } else { round.len() + 1 };
        let trial_gas = round
            .iter()
            .map(|index| group.siblings()[*index].transaction.gas_limit())
            .fold(0_u64, u64::saturating_add);
        trial_gas.saturating_add(config.probe_gas.saturating_mul(probes as u64))
    }

    /// Rejection-cache, static DA/gas, missing-metering and simulated resource admission for
    /// one sibling.
    pub fn sibling_precheck(
        &self,
        info: &ExecutionInfo,
        sibling: &CandidateSibling,
        limits: &CandidateBlockLimits,
    ) -> Result<(), Ineligibility> {
        let hash = sibling.hash();
        if self.builder_config.rejection_cache.is_rejected(&hash) {
            return Err(Ineligibility::PermanentlyExcluded);
        }
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
        let resource_metering = &self.builder_config.resource_metering;
        let (simulated, admission) =
            resource_metering.check_simulated_usage(&hash, &info.resource_metering_usage);
        if simulated.is_none() && resource_metering.requires_simulated_sample() {
            return Err(Ineligibility::Unmetered);
        }
        if admission.should_exclude() {
            return Err(Ineligibility::SimulatedResource { permanent: admission.is_permanent() });
        }
        Ok(())
    }

    /// Returns a sibling's estimated DA size, as the pool would report it.
    pub fn sibling_da_size(sibling: &CandidateSibling) -> u64 {
        BasePooledTransaction::<BaseTransactionSigned>::new(
            sibling.transaction.clone(),
            sibling.encoded.len(),
        )
        .estimated_da_size()
    }

    /// Converts a sibling into the builder's consensus transaction type.
    pub fn consensus_sibling(
        sibling: &CandidateSibling,
    ) -> WithEncoded<Recovered<TxTy<Evm::Primitives>>> {
        let transaction = TxTy::<Evm::Primitives>::from(sibling.transaction.inner().clone());
        WithEncoded::new(
            sibling.encoded.clone(),
            Recovered::new_unchecked(transaction, sibling.transaction.signer()),
        )
    }

    /// Returns why the round must stop before starting more work, if it must.
    pub fn round_stop(
        &self,
        scan: &CandidateScan,
        config: &CandidateBuilderConfig,
    ) -> Option<RoundEnd> {
        if self.cancel.is_cancelled() {
            return Some(RoundEnd::Cancelled);
        }
        if self.is_denim_active() && self.cancel.is_finalization_requested() {
            return Some(RoundEnd::Finalized);
        }
        if !scan.has_time(config) {
            return Some(RoundEnd::OutOfTime);
        }
        None
    }

    /// Runs every sibling in the round as an uncommitted trial and scores it.
    ///
    /// Checks cancellation, finalization and the build deadline before every trial and probe.
    /// `round.work` holds the work consumed, also when the round stops early or fails.
    pub fn evaluate_round<Builder>(
        &self,
        cumulative: &[u128],
        builder: &mut Builder,
        turn: CandidateTurn<'_>,
        round: &mut CandidateRound,
        scan: &CandidateScan,
    ) -> Result<RoundEnd, PayloadBuilderError>
    where
        Builder: BlockBuilder<Primitives = Evm::Primitives>,
        <<Builder::Executor as BlockExecutor>::Evm as AlloyEvm>::DB: Database,
    {
        let group = turn.group;
        let config = turn.config;
        let rule = &group.envelope().rule;
        let evm_env = if rule.token.is_zero() { None } else { Some(self.probe_env()?) };
        if let Some(end) = self.round_stop(scan, config) {
            return Ok(end);
        }
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
                round.work = round.work.saturating_add(gas);
                result
            }
        };
        for position in 0..round.siblings.len() {
            let index = round.siblings[position];
            if let Some(end) = self.round_stop(scan, config) {
                return Ok(end);
            }
            let sibling = &group.siblings()[index];
            let trial =
                self.trial(builder, sibling, rule.recipient, evm_env.is_some(), cumulative)?;
            let gas_used = match trial.outcome {
                Ok(gas_used) => gas_used,
                Err(reason) => {
                    // Invalid siblings consume validation time without EVM gas.
                    round.work = round.work.saturating_add(match reason {
                        Ineligibility::Invalid => config.invalid_attempt_gas,
                        _ => trial.gas_used,
                    });
                    round.outcomes[index] = TrialOutcome::Ineligible(reason);
                    continue;
                }
            };
            round.work = round.work.saturating_add(gas_used);
            // Executed and charged; a stop or error before scoring leaves it interrupted.
            round.outcomes[index] = TrialOutcome::Ineligible(Ineligibility::Interrupted);
            let before = match before {
                Ok(before) => before,
                Err(reason) => {
                    // A failed before-probe makes every trial in the round ineligible.
                    round.outcomes[index] = TrialOutcome::Ineligible(reason);
                    continue;
                }
            };
            let after = match &evm_env {
                None => Ok(trial.recipient_balance.unwrap_or(before)),
                Some(env) => {
                    if let Some(end) = self.round_stop(scan, config) {
                        return Ok(end);
                    }
                    let (result, gas) = self.probe_balance(
                        builder,
                        env.clone(),
                        trial.state.as_ref(),
                        config,
                        rule,
                    )?;
                    round.work = round.work.saturating_add(gas);
                    result
                }
            };
            round.outcomes[index] =
                match after.and_then(|after| ScoreRead::delta(before, after, rule.min_delta)) {
                    Ok(delta) => TrialOutcome::Eligible { delta, gas_used },
                    Err(reason) => TrialOutcome::Ineligible(reason),
                };
        }
        Ok(RoundEnd::Completed)
    }

    /// Executes one sibling without committing and applies executed resource admission.
    pub fn trial<Builder>(
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
        if simulated.is_none() && resource_metering.requires_simulated_sample() {
            return Ok(SiblingTrial {
                outcome: Err(Ineligibility::Unmetered),
                ..SiblingTrial::invalid()
            });
        }
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
                    gas_used,
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
    pub fn probe_env(&self) -> Result<EvmEnvFor<Evm>, PayloadBuilderError> {
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
    /// The probe is a gas-capped, zero-value, fee-free top-level `STATICCALL` from
    /// `address(0)`: a deposit-shaped carrier (no fee, nonce or balance checks; standard
    /// intrinsic gas and EIP-2929 warm set) whose top-level frame [`StaticTopLevelCall`] makes
    /// static. Returns the decoded balance or the trial ineligibility, plus the probe gas used.
    pub fn probe_balance<Builder>(
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
        let mut evm = self.evm_config.evm_with_env_and_inspector(
            overlay,
            evm_env,
            StaticTopLevelCall::default(),
        );
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
    pub fn nonce_already_used<Builder>(&self, builder: &mut Builder, group: &CandidateGroup) -> bool
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
    ///
    /// A transaction-scope executed exclusion is recorded against this sibling in the ledger,
    /// never in the rejection cache. When `scan` tracks committed state, the committed diff is
    /// kept for the builder's predicate wakeups.
    pub fn commit_sibling<Builder>(
        &self,
        info: &mut ExecutionInfo,
        builder: &mut Builder,
        turn: CandidateTurn<'_>,
        index: usize,
        scan: &mut CandidateScan,
    ) -> Result<SiblingCommit, PayloadBuilderError>
    where
        Builder: BlockBuilder<Primitives = Evm::Primitives>,
    {
        let group = turn.group;
        let limits = turn.limits;
        let track_state = scan.track_committed_state;
        let mut committed_state = None;
        let sibling = &group.siblings()[index];
        let hash = sibling.hash();
        let resource_metering = &self.builder_config.resource_metering;
        let (simulated, _) =
            resource_metering.check_simulated_usage(&hash, &info.resource_metering_usage);
        // Each lookup is fresh: a sample evicted since the trial must not commit unmetered.
        if simulated.is_none() && resource_metering.requires_simulated_sample() {
            return Ok(SiblingCommit::Unmetered);
        }
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
                    if track_state {
                        committed_state = Some(result_and_state.state.clone());
                    }
                    CommitChanges::Yes
                }
            }) {
                Ok(Some(gas_output)) => gas_output,
                Ok(None) => {
                    let permanent =
                        executed_decision.as_ref().is_some_and(|decision| decision.is_permanent());
                    if permanent {
                        turn.config
                            .runtime
                            .ledger
                            .mark_permanently_ineligible(group.group_hash(), index);
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

        scan.committed_state = committed_state;
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
