//! Per-candidate build policy for the flashblock build loop.
//!
//! Extracts the resource metering that is otherwise interleaved into the build loop into a
//! standalone [`MeteringBuildPolicy`]. Given a candidate transaction, the policy consults the
//! [`MeteringProvider`](crate::MeteringProvider) to decide whether the candidate must wait for
//! metering data, and otherwise produces the [`TxResources`] estimate that the loop's generic,
//! metering-agnostic limit check consumes. This is the default build behaviour; the loop stays
//! oblivious to how resources are estimated.

use core::{fmt::Debug, time::Duration};
use std::time::{SystemTime, UNIX_EPOCH};

use alloy_consensus::Transaction;
use alloy_primitives::{Address, TxHash};
use base_bundles::MeterBundleResponse;
use base_common_consensus::BaseTransactionSigned;
use reth_primitives_traits::Recovered;

use crate::{
    BuilderMetrics, ExecutionInfo, ExecutionMeteringLimitExceeded, ExecutionMeteringMode,
    ResourceLimits, SharedMeteringProvider, TxResources, TxnExecutionError,
};

/// Default build policy: estimates per-candidate resource usage from metering data.
///
/// Owns the metering-derived logic previously inlined in the build loop — the skip-if-pending
/// wait, predicted-execution-time extraction, and the state-root-gas formula — so the loop keeps
/// only the generic limit check.
#[derive(Debug, Clone)]
pub struct MeteringBuildPolicy {
    metering_provider: SharedMeteringProvider,
    state_root_gas_coefficient: f64,
    state_root_gas_anchor_us: u128,
    metering_wait_duration: Option<Duration>,
    metering_mode: ExecutionMeteringMode,
}

/// Outcome of consulting a [`MeteringBuildPolicy`] for a single candidate.
#[derive(Debug, Clone)]
pub enum MeteringEstimate {
    /// Metering is enabled, no data has arrived yet, and the candidate is younger than the
    /// configured wait duration. The loop should skip it so it can be retried once data lands.
    Pending,
    /// The candidate is ready to attempt; carries its resource estimate and the raw metering
    /// response for downstream accuracy metrics and rejection auditing. Boxed to keep the enum
    /// small, since the metering response dwarfs the empty [`Pending`](Self::Pending) variant.
    Ready(Box<MeteredCandidate>),
}

/// A candidate that passed the metering wait gate, with its resource estimate.
#[derive(Debug, Clone)]
pub struct MeteredCandidate {
    /// Estimated resource usage, consumed by the loop's generic limit check.
    pub resources: TxResources,
    /// Raw metering response, if any, for prediction-accuracy metrics and rejection auditing.
    pub metering: Option<MeterBundleResponse>,
    /// Predicted state root time in microseconds, if metering data was available.
    pub predicted_state_root_time_us: Option<u128>,
}

impl MeteringBuildPolicy {
    /// Creates a policy from the metering provider and state-root-gas parameters.
    pub fn new(
        metering_provider: SharedMeteringProvider,
        state_root_gas_coefficient: f64,
        state_root_gas_anchor_us: u128,
        metering_wait_duration: Option<Duration>,
        metering_mode: ExecutionMeteringMode,
    ) -> Self {
        Self {
            metering_provider,
            state_root_gas_coefficient,
            state_root_gas_anchor_us,
            metering_wait_duration,
            metering_mode,
        }
    }

    /// Estimates the resource usage for a candidate transaction.
    ///
    /// Returns [`MeteringEstimate::Pending`] when metering is enabled but data has not yet arrived
    /// for a transaction younger than the configured wait duration — the loop should skip the
    /// candidate and retry it later. Otherwise returns [`MeteringEstimate::Ready`] with the
    /// [`TxResources`] estimate, populated from metering data when available.
    pub fn estimate_resources(
        &self,
        tx_hash: TxHash,
        da_size: u64,
        gas_limit: u64,
        uncompressed_size: u64,
        received_at_ms: u128,
    ) -> MeteringEstimate {
        let metering = self.metering_provider.get(&tx_hash);

        // Skip transactions that are too young and don't have metering data yet.
        if self.metering_provider.is_enabled()
            && metering.is_none()
            && let Some(wait_duration) = self.metering_wait_duration
        {
            let now_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0);
            let tx_age_ms = now_ms.saturating_sub(received_at_ms);
            if tx_age_ms < wait_duration.as_millis() {
                BuilderMetrics::metering_data_pending_skip().increment(1);
                self.metering_provider.skip(&tx_hash);
                return MeteringEstimate::Pending;
            }
        }

        // Derive the metered fields in a single pass over the metering data. Without metering
        // data, none of the predictions apply.
        let (predicted_execution_time_us, predicted_state_root_time_us, state_root_gas) =
            metering.as_ref().map_or((None, None, None), |m| {
                // sr_gas = gas_used × (1 + K × max(0, SR_ms - anchor_ms))
                let excess_us = m.state_root_time_us.saturating_sub(self.state_root_gas_anchor_us);
                let excess_ms = excess_us as f64 / 1000.0;
                let multiplier = 1.0 + self.state_root_gas_coefficient * excess_ms;
                let state_root_gas = (m.total_gas_used as f64 * multiplier) as u64;
                (
                    Some(m.total_execution_time_us),
                    Some(m.state_root_time_us),
                    Some(state_root_gas),
                )
            });

        MeteringEstimate::Ready(Box::new(MeteredCandidate {
            resources: TxResources {
                da_size,
                gas_limit,
                execution_time_us: predicted_execution_time_us,
                state_root_gas,
                uncompressed_size,
            },
            metering,
            predicted_state_root_time_us,
        }))
    }
}

/// Immutable per-flashblock context handed to a [`BuildPolicy`] when a flashblock begins.
#[derive(Debug, Clone)]
pub struct FlashblockCtx {
    /// Number of the block being built.
    pub block_number: u64,
    /// Timestamp of the block being built, in seconds.
    pub block_timestamp: u64,
    /// Base fee of the block being built.
    pub base_fee: u64,
}

/// Live cumulative block state a [`FlashblockBuilderPolicy`] consults to admit candidates.
///
/// Borrows the loop's accumulated [`ExecutionInfo`] and the block [`ResourceLimits`] so the policy
/// can apply the same hard-limit filter the loop enforces authoritatively before committing.
#[derive(Debug)]
pub struct BuildBudget<'a> {
    /// Accumulated execution state for the block so far.
    pub info: &'a ExecutionInfo,
    /// Resource limits for the block.
    pub limits: &'a ResourceLimits,
}

/// A validated candidate produced by a [`CandidateSource`].
///
/// The source has already applied bundle-window gating (target block, expiry, not-yet-valid), so
/// every `SourcedCandidate` is within its bundle window; the recovered transaction and its sizes
/// are materialized for the policy's resource estimate.
#[derive(Debug, Clone)]
pub struct SourcedCandidate {
    /// Recovered consensus transaction.
    pub tx: Recovered<BaseTransactionSigned>,
    /// Transaction hash.
    pub tx_hash: TxHash,
    /// Estimated DA size.
    pub da_size: u64,
    /// Raw EIP-2718 encoded size in bytes.
    pub uncompressed_size: u64,
    /// Time the transaction was received, in milliseconds since the Unix epoch.
    pub received_at_ms: u128,
}

/// Object-safe stream of candidates a policy drains during a flashblock.
///
/// The open adapter drains the transaction pool's best-transactions iterator, applying
/// bundle-window gating before yielding so bundle-invalid transactions are skipped without being
/// materialized. A closed policy can layer additional candidate sources (e.g. a bundle service)
/// behind the same interface.
pub trait CandidateSource: Debug {
    /// Returns the next validated candidate, or `None` when the stream is exhausted.
    fn next_candidate(&mut self) -> Option<SourcedCandidate>;

    /// Marks the given sender/nonce invalid so its descendants are skipped by the source.
    fn mark_invalid(&mut self, sender: Address, nonce: u64);
}

/// A candidate a policy has admitted for the build loop to execute.
#[derive(Debug, Clone)]
pub struct NextCandidate {
    /// Recovered consensus transaction to execute.
    pub tx: Recovered<BaseTransactionSigned>,
    /// Transaction hash.
    pub tx_hash: TxHash,
    /// Resource estimate; the loop re-checks it against the hard limits before committing.
    pub resources: TxResources,
    /// Raw metering response, if any, for prediction metrics and inclusion accounting.
    pub metering: Option<MeterBundleResponse>,
    /// Predicted state root time in microseconds, if metering data was available.
    pub predicted_state_root_time_us: Option<u128>,
    /// Set when the candidate exceeded a metering limit but was admitted anyway under dry-run
    /// mode; the loop records the shadow rejection metric and warning.
    pub shadow_limit: Option<ExecutionMeteringLimitExceeded>,
}

/// A candidate the policy rejected before execution, surfaced so the loop records diagnostics.
///
/// The policy has already marked the candidate invalid on its source; this carries what the loop
/// needs to reproduce its diagnostic, metric, and audit recording for the rejection.
#[derive(Debug, Clone)]
pub struct RejectedCandidate {
    /// Recovered consensus transaction (for the effective priority-fee metric label).
    pub tx: Recovered<BaseTransactionSigned>,
    /// Transaction hash.
    pub tx_hash: TxHash,
    /// Estimated DA size (for diagnostic logging).
    pub da_size: u64,
    /// The rejection reason.
    pub error: TxnExecutionError,
    /// Raw metering response, if any, for the rejected-transaction audit trail.
    pub metering: Option<MeterBundleResponse>,
}

/// One step of draining a flashblock's candidates.
#[derive(Debug)]
pub enum PolicyStep {
    /// A candidate admitted for the loop to execute.
    Admit(Box<NextCandidate>),
    /// A candidate the policy rejected before execution; the loop records diagnostics and
    /// continues. The policy has already invalidated it on its source.
    Reject(Box<RejectedCandidate>),
    /// The policy has no more candidates for this flashblock.
    Done,
}

/// Outcome of a candidate the loop executed, reported back so the policy can update its source.
#[derive(Debug, Clone, Copy)]
pub enum CandidateOutcome {
    /// The candidate was committed to the block.
    Committed,
    /// The candidate was dropped and its sender's descendants should be skipped (e.g. an invalid
    /// transaction or an over-gas rejection).
    DroppedInvalidate,
    /// The candidate was dropped but its descendants remain valid (e.g. nonce too low).
    DroppedRetainDescendants,
}

/// A build policy: supplies and gates the candidates the build loop attempts, per flashblock.
///
/// Injected on the builder configuration as `Arc<dyn BuildPolicy>`, mirroring the metering
/// provider seam. [`begin_flashblock`](Self::begin_flashblock) produces a per-flashblock driver
/// that owns candidate sourcing, so a closed implementation can source from a bundle service in
/// addition to the pool without the open loop naming any of it.
pub trait BuildPolicy: Send + Sync + 'static {
    /// Begins a flashblock, taking ownership of the candidate stream for its duration.
    fn begin_flashblock<'a>(
        &self,
        source: Box<dyn CandidateSource + 'a>,
        ctx: &FlashblockCtx,
    ) -> Box<dyn FlashblockBuilderPolicy + 'a>;
}

/// Per-flashblock driver returned by [`BuildPolicy::begin_flashblock`].
pub trait FlashblockBuilderPolicy {
    /// Advances the candidate stream: yields the next admitted candidate, a rejection for the
    /// loop to record, or [`PolicyStep::Done`] when the flashblock is exhausted. The policy
    /// invalidates rejected candidates on its own source; the loop re-checks admitted candidates
    /// against the hard limits before committing, remaining authoritative on block limits.
    fn next(&mut self, budget: &BuildBudget<'_>) -> PolicyStep;

    /// Reports the outcome of the last admitted candidate so the policy can invalidate its
    /// source when a post-execution rejection drops the sender's descendants.
    fn observe(&mut self, outcome: CandidateOutcome);

    /// End-of-flashblock hook for final policy bookkeeping.
    fn finish(&mut self) {}
}

impl BuildPolicy for MeteringBuildPolicy {
    fn begin_flashblock<'a>(
        &self,
        source: Box<dyn CandidateSource + 'a>,
        _ctx: &FlashblockCtx,
    ) -> Box<dyn FlashblockBuilderPolicy + 'a> {
        Box::new(MeteringFlashblockPolicy { metering: self.clone(), source, last: None })
    }
}

/// Per-flashblock driver for [`MeteringBuildPolicy`].
///
/// Drains its [`CandidateSource`], applies the metering-free hard-limit filter, estimates
/// resources from metering data, and applies the soft metering limits (rejecting in enforce mode,
/// admitting with a shadow marker in dry-run) before yielding a [`NextCandidate`].
#[derive(Debug)]
pub struct MeteringFlashblockPolicy<'a> {
    metering: MeteringBuildPolicy,
    source: Box<dyn CandidateSource + 'a>,
    /// Sender/nonce of the last yielded candidate, for `observe`'s descendant invalidation.
    last: Option<(Address, u64)>,
}

impl FlashblockBuilderPolicy for MeteringFlashblockPolicy<'_> {
    fn next(&mut self, budget: &BuildBudget<'_>) -> PolicyStep {
        let Some(cand) = self.source.next_candidate() else { return PolicyStep::Done };
        let sender = cand.tx.signer();
        let nonce = cand.tx.nonce();
        self.last = Some((sender, nonce));
        let gas_limit = cand.tx.gas_limit();

        // Hard limits: cheap, metering-free. Reject before taking the metering lock.
        let hard = TxResources {
            da_size: cand.da_size,
            gas_limit,
            uncompressed_size: cand.uncompressed_size,
            ..Default::default()
        };
        if let Err(error) = budget.info.is_tx_over_hard_limits(&hard, budget.limits) {
            self.source.mark_invalid(sender, nonce);
            return PolicyStep::Reject(Box::new(RejectedCandidate {
                tx: cand.tx,
                tx_hash: cand.tx_hash,
                da_size: cand.da_size,
                error,
                metering: None,
            }));
        }

        // Metering estimate — only for candidates that pass the hard limits.
        let candidate = match self.metering.estimate_resources(
            cand.tx_hash,
            cand.da_size,
            gas_limit,
            cand.uncompressed_size,
            cand.received_at_ms,
        ) {
            MeteringEstimate::Pending => {
                self.source.mark_invalid(sender, nonce);
                return PolicyStep::Reject(Box::new(RejectedCandidate {
                    tx: cand.tx,
                    tx_hash: cand.tx_hash,
                    da_size: cand.da_size,
                    error: TxnExecutionError::MeteringDataPending,
                    metering: None,
                }));
            }
            MeteringEstimate::Ready(candidate) => *candidate,
        };

        // Soft metering limits: reject in enforce mode; admit with a shadow marker in dry-run.
        let shadow_limit =
            match budget.info.is_tx_over_metering_limits(&candidate.resources, budget.limits) {
                Ok(()) => None,
                Err(limit_err) if self.metering.metering_mode.is_dry_run() => Some(limit_err),
                Err(limit_err) => {
                    self.source.mark_invalid(sender, nonce);
                    return PolicyStep::Reject(Box::new(RejectedCandidate {
                        tx: cand.tx,
                        tx_hash: cand.tx_hash,
                        da_size: cand.da_size,
                        error: TxnExecutionError::from(limit_err),
                        metering: candidate.metering,
                    }));
                }
            };

        PolicyStep::Admit(Box::new(NextCandidate {
            tx: cand.tx,
            tx_hash: cand.tx_hash,
            resources: candidate.resources,
            metering: candidate.metering,
            predicted_state_root_time_us: candidate.predicted_state_root_time_us,
            shadow_limit,
        }))
    }

    fn observe(&mut self, outcome: CandidateOutcome) {
        if matches!(outcome, CandidateOutcome::DroppedInvalidate)
            && let Some((sender, nonce)) = self.last
        {
            self.source.mark_invalid(sender, nonce);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::{MeteringProvider, NoopMeteringProvider};

    use super::*;

    /// Metering provider returning a fixed response, with a configurable enabled flag.
    #[derive(Debug)]
    struct FixedMeteringProvider {
        enabled: bool,
        response: Option<MeterBundleResponse>,
    }

    impl MeteringProvider for FixedMeteringProvider {
        fn get(&self, _tx_hash: &TxHash) -> Option<MeterBundleResponse> {
            self.response.clone()
        }

        fn is_enabled(&self) -> bool {
            self.enabled
        }
    }

    fn now_ms() -> u128 {
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis()
    }

    fn ready(estimate: MeteringEstimate) -> MeteredCandidate {
        match estimate {
            MeteringEstimate::Ready(candidate) => *candidate,
            MeteringEstimate::Pending => panic!("expected Ready, got Pending"),
        }
    }

    #[test]
    fn no_metering_data_yields_empty_estimate() {
        let policy =
            MeteringBuildPolicy::new(Arc::new(NoopMeteringProvider), 0.02, 5_000, None, ExecutionMeteringMode::Off);

        let candidate = ready(policy.estimate_resources(TxHash::default(), 100, 21_000, 120, 0));

        assert_eq!(candidate.resources.da_size, 100);
        assert_eq!(candidate.resources.gas_limit, 21_000);
        assert_eq!(candidate.resources.uncompressed_size, 120);
        assert_eq!(candidate.resources.execution_time_us, None);
        assert_eq!(candidate.resources.state_root_gas, None);
        assert_eq!(candidate.predicted_state_root_time_us, None);
        assert!(candidate.metering.is_none());
    }

    #[test]
    fn state_root_gas_uses_gas_time_and_anchor() {
        // gas_used=1_000_000, sr=15ms, anchor=5ms => excess=10ms, K=0.02 => x1.2 => 1_200_000.
        let response = MeterBundleResponse {
            total_gas_used: 1_000_000,
            state_root_time_us: 15_000,
            total_execution_time_us: 42,
            ..Default::default()
        };
        let provider = FixedMeteringProvider { enabled: true, response: Some(response) };
        let policy = MeteringBuildPolicy::new(Arc::new(provider), 0.02, 5_000, None, ExecutionMeteringMode::Off);

        let candidate = ready(policy.estimate_resources(TxHash::default(), 0, 1_000_000, 0, 0));

        assert_eq!(candidate.resources.state_root_gas, Some(1_200_000));
        assert_eq!(candidate.resources.execution_time_us, Some(42));
        assert_eq!(candidate.predicted_state_root_time_us, Some(15_000));
    }

    #[test]
    fn state_root_time_below_anchor_pays_one_to_one() {
        // sr=3ms < anchor=5ms => no penalty => sr_gas == gas_used.
        let response = MeterBundleResponse {
            total_gas_used: 500_000,
            state_root_time_us: 3_000,
            ..Default::default()
        };
        let provider = FixedMeteringProvider { enabled: true, response: Some(response) };
        let policy = MeteringBuildPolicy::new(Arc::new(provider), 0.02, 5_000, None, ExecutionMeteringMode::Off);

        let candidate = ready(policy.estimate_resources(TxHash::default(), 0, 500_000, 0, 0));

        assert_eq!(candidate.resources.state_root_gas, Some(500_000));
    }

    #[test]
    fn pending_when_enabled_young_and_no_data() {
        let provider = FixedMeteringProvider { enabled: true, response: None };
        let policy = MeteringBuildPolicy::new(
            Arc::new(provider),
            0.02,
            5_000,
            Some(Duration::from_secs(60)),
            ExecutionMeteringMode::Off,
        );

        let estimate = policy.estimate_resources(TxHash::default(), 0, 21_000, 0, now_ms());

        assert!(matches!(estimate, MeteringEstimate::Pending));
    }

    #[test]
    fn ready_when_enabled_old_and_no_data() {
        let provider = FixedMeteringProvider { enabled: true, response: None };
        let policy = MeteringBuildPolicy::new(
            Arc::new(provider),
            0.02,
            5_000,
            Some(Duration::from_millis(1)),
            ExecutionMeteringMode::Off,
        );

        // received_at = 0 => effectively ancient => past the wait window.
        let candidate = ready(policy.estimate_resources(TxHash::default(), 0, 21_000, 0, 0));

        assert_eq!(candidate.resources.state_root_gas, None);
    }

    #[test]
    fn ready_when_disabled_even_without_data() {
        // The wait gate only applies when metering is enabled.
        let provider = FixedMeteringProvider { enabled: false, response: None };
        let policy = MeteringBuildPolicy::new(
            Arc::new(provider),
            0.02,
            5_000,
            Some(Duration::from_secs(60)),
            ExecutionMeteringMode::Off,
        );

        let estimate = policy.estimate_resources(TxHash::default(), 0, 21_000, 0, now_ms());

        assert!(matches!(estimate, MeteringEstimate::Ready(_)));
    }
}
