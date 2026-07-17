//! Admission gates for the flashblock build loop.
//!
//! A [`Gate`] inspects a [`Candidate`] and returns a [`GateVerdict`] — either admitting it or
//! rejecting it with a [`GateRejection`] the [`OutcomeReporter`](super::reporter::OutcomeReporter)
//! knows how to render. The build walk runs the gates in order and stops at the first rejection.
//!
//! The open build walk composes four gates:
//! - [`BundleGate`] — bundle-window validity (target block, expiry, not-yet-valid).
//! - [`ManifestGate`] — revalidates EIP-8130 authorization manifests against on-chain state.
//! - [`ResourceLimitsGate`] — metering estimation (and its wait/skip), the always-enforced hard
//!   limits, and the dry-run/enforce metering limit.
//! - [`SequencerGate`] — rejects blob/deposit transactions sourced from the pool.

use std::time::{Duration, SystemTime};

use alloy_eips::Typed2718;
use base_bundles::MeterBundleResponse;
use base_execution_txpool::{BundleTransaction, GuardMetrics};
use revm::Database;
use tracing::warn;

use super::candidate_source::Candidate;
use crate::{
    BuilderMetrics, ExecutionInfo, ExecutionMeteringLimitExceeded, ExecutionMeteringMode,
    ResourceLimits, SharedMeteringProvider, TxnExecutionError,
};

/// The outcome of evaluating a candidate against a [`Gate`].
#[derive(Debug)]
pub enum GateVerdict {
    /// The candidate passes this gate.
    Admit,
    /// The candidate is rejected; the reporter renders the rejection.
    Reject(GateRejection),
}

/// A gate rejection, carrying everything the reporter needs to record and emit it.
#[derive(Debug)]
pub enum GateRejection {
    /// Bundle targets a block other than the one being built.
    BundleWrongTarget {
        /// The bundle's requested target block.
        target: u64,
        /// The block being built.
        current: u64,
    },
    /// Bundle validity window has already passed.
    BundleExpired {
        /// The block timestamp checked against.
        block_timestamp: u64,
    },
    /// Bundle validity window has not started yet.
    BundleNotYetValid {
        /// The block timestamp checked against.
        block_timestamp: u64,
    },
    /// Metering is enabled but data has not yet arrived and the candidate is within the wait window.
    MeteringPending {
        /// Age of the candidate in milliseconds.
        tx_age_ms: u128,
        /// Configured wait duration in milliseconds.
        wait_duration_ms: u128,
    },
    /// An always-enforced hard limit exceeded (DA, footprint, gas, uncompressed size).
    Limit(TxnExecutionError),
    /// A blob or deposit transaction, which must never be sourced from the pool.
    Sequencer,
    /// An EIP-8130 authorization manifest that no longer revalidates against on-chain state.
    ManifestStale {
        /// The reason the manifest is stale (from [`ManifestStale::cause`]).
        ///
        /// [`ManifestStale::cause`]: base_execution_txpool::ManifestStale::cause
        cause: &'static str,
    },
    /// A metering limit exceeded in enforce mode.
    MeteringLimit {
        /// The exceeded limit.
        limit: ExecutionMeteringLimitExceeded,
        /// Raw metering response for the rejected-tx audit trail. Boxed to keep the enum small,
        /// since the metering response dwarfs the other rejection variants.
        resource_usage: Option<Box<MeterBundleResponse>>,
    },
}

/// A build-loop admission gate. Implementors inspect the candidate and admit or reject it.
///
/// Evaluation is given `&mut db` so a gate can revalidate against current on-chain state (used by
/// [`ManifestGate`]); gates that don't need it simply ignore it.
pub trait Gate {
    /// Evaluates the candidate, optionally enriching it (e.g. with a resource estimate).
    fn evaluate(
        &self,
        candidate: &mut Candidate,
        info: &ExecutionInfo,
        limits: &ResourceLimits,
        db: &mut impl Database,
    ) -> GateVerdict;

    /// Chains `next` to run after this gate, producing a single compound gate. The chain admits
    /// only if both admit and returns the first rejection, so `a.then(b).then(c)` composes the
    /// admission sequence with static dispatch.
    fn then<G: Gate>(self, next: G) -> Chain<Self, G>
    where
        Self: Sized,
    {
        Chain { first: self, next }
    }
}

/// Two gates run in sequence: `next` is evaluated only if `first` admits. Produced by
/// [`Gate::then`].
#[derive(Debug, Clone, Copy)]
pub struct Chain<A, B> {
    first: A,
    next: B,
}

impl<A: Gate, B: Gate> Gate for Chain<A, B> {
    fn evaluate(
        &self,
        candidate: &mut Candidate,
        info: &ExecutionInfo,
        limits: &ResourceLimits,
        db: &mut impl Database,
    ) -> GateVerdict {
        match self.first.evaluate(candidate, info, limits, &mut *db) {
            GateVerdict::Admit => self.next.evaluate(candidate, info, limits, db),
            reject => reject,
        }
    }
}

/// Rejects bundle transactions whose validity window does not include the block being built.
#[derive(Debug, Clone, Copy)]
pub struct BundleGate {
    block_number: u64,
    block_timestamp: u64,
}

impl BundleGate {
    /// Creates a bundle gate for the block being built.
    pub const fn new(block_number: u64, block_timestamp: u64) -> Self {
        Self { block_number, block_timestamp }
    }
}

impl Gate for BundleGate {
    fn evaluate(
        &self,
        candidate: &mut Candidate,
        _info: &ExecutionInfo,
        _limits: &ResourceLimits,
        _db: &mut impl Database,
    ) -> GateVerdict {
        let bundle = &candidate.bundle;

        if let Some(target) = bundle.target_block_number()
            && target != self.block_number
        {
            return GateVerdict::Reject(GateRejection::BundleWrongTarget {
                target,
                current: self.block_number,
            });
        }

        if bundle.is_bundle_expired(self.block_number, self.block_timestamp) {
            return GateVerdict::Reject(GateRejection::BundleExpired {
                block_timestamp: self.block_timestamp,
            });
        }

        if bundle.is_bundle_not_yet_valid(self.block_timestamp) {
            return GateVerdict::Reject(GateRejection::BundleNotYetValid {
                block_timestamp: self.block_timestamp,
            });
        }

        GateVerdict::Admit
    }
}

/// Revalidates a candidate's EIP-8130 authorization manifest against current on-chain state,
/// dropping it (with [`GateRejection::ManifestStale`]) when the authorization has gone stale.
///
/// Only transactions carrying a manifest are affected; the check is a no-op when prechecking is
/// disabled or the candidate has no manifest.
#[derive(Debug, Clone, Copy)]
pub struct ManifestGate {
    enabled: bool,
    block_timestamp: u64,
}

impl ManifestGate {
    /// Creates a manifest gate for the block being built.
    pub const fn new(enabled: bool, block_timestamp: u64) -> Self {
        Self { enabled, block_timestamp }
    }
}

impl Gate for ManifestGate {
    fn evaluate(
        &self,
        candidate: &mut Candidate,
        _info: &ExecutionInfo,
        _limits: &ResourceLimits,
        db: &mut impl Database,
    ) -> GateVerdict {
        if self.enabled
            && let Some(manifest) = &candidate.watch_manifest
            && let Err(stale) = manifest.revalidate(db, self.block_timestamp)
        {
            GuardMetrics::record_builder_precheck_drop(&stale);
            return GateVerdict::Reject(GateRejection::ManifestStale { cause: stale.cause() });
        }
        GateVerdict::Admit
    }
}

/// Estimates a candidate's resources from metering-service data, then enforces the resource limits.
///
/// On evaluation this gate:
/// 1. looks up metering data and, when metering is enabled and data has not arrived within the wait
///    window, defers the candidate (rejecting with [`GateRejection::MeteringPending`]);
/// 2. enriches the candidate's resource estimate with the predicted execution time;
/// 3. enforces the always-on hard limits ([`GateRejection::Limit`]);
/// 4. enforces the metering limit under the configured mode — recording the would-reject metric in
///    both modes, admitting in dry-run, and rejecting with [`GateRejection::MeteringLimit`] in
///    enforce mode.
#[derive(Debug, Clone, Copy)]
pub struct ResourceLimitsGate<'a> {
    provider: &'a SharedMeteringProvider,
    wait_duration: Option<Duration>,
    metering_mode: ExecutionMeteringMode,
}

impl<'a> ResourceLimitsGate<'a> {
    /// Creates a resource-limits gate over the given metering provider, wait duration, and mode.
    pub const fn new(
        provider: &'a SharedMeteringProvider,
        wait_duration: Option<Duration>,
        metering_mode: ExecutionMeteringMode,
    ) -> Self {
        Self { provider, wait_duration, metering_mode }
    }
}

impl Gate for ResourceLimitsGate<'_> {
    fn evaluate(
        &self,
        candidate: &mut Candidate,
        info: &ExecutionInfo,
        limits: &ResourceLimits,
        _db: &mut impl Database,
    ) -> GateVerdict {
        let resource_usage = self.provider.get(&candidate.tx_hash);

        // Skip transactions that are too young and don't have metering data yet.
        if self.provider.is_enabled()
            && resource_usage.is_none()
            && let Some(wait_duration) = self.wait_duration
        {
            let now_ms = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0);
            let tx_age_ms = now_ms.saturating_sub(candidate.received_at_ms);
            if tx_age_ms < wait_duration.as_millis() {
                self.provider.skip(&candidate.tx_hash);
                return GateVerdict::Reject(GateRejection::MeteringPending {
                    tx_age_ms,
                    wait_duration_ms: wait_duration.as_millis(),
                });
            }
        }

        // Enrich the estimate with the predicted execution time.
        let predicted_execution_time_us =
            resource_usage.as_ref().map(|m| m.total_execution_time_us);
        candidate.resources.execution_time_us = predicted_execution_time_us;
        candidate.resource_usage = resource_usage;

        // Hard limits are always enforced.
        if let Err(err) = info.is_tx_over_hard_limits(&candidate.resources, limits) {
            return GateVerdict::Reject(GateRejection::Limit(err));
        }

        // Metering limits are checked under the configured dry-run/enforce mode.
        if let Err(limit) = info.is_tx_over_metering_limits(&candidate.resources, limits) {
            BuilderMetrics::resource_limit_would_reject_total().increment(1);
            let ExecutionMeteringLimitExceeded::TransactionExecutionTime(_, _) = limit;
            BuilderMetrics::tx_execution_time_exceeded_total().increment(1);

            let dry_run = self.metering_mode.is_dry_run();
            warn!(
                target: "payload_builder",
                message = if dry_run {
                    "Metering throttle: transaction would be rejected (dry-run)"
                } else {
                    "Metering throttle: transaction rejected"
                },
                tx_hash = ?candidate.tx_hash,
                limit = %limit,
                priority_fee = candidate.priority_fee,
                dry_run,
            );

            if !dry_run {
                return GateVerdict::Reject(GateRejection::MeteringLimit {
                    limit,
                    // Move the metering response out of the candidate (which is dropped right after
                    // this returns) rather than cloning its `Vec<TransactionResult>`.
                    resource_usage: candidate.resource_usage.take().map(Box::new),
                });
            }
        }

        // Admitted: record the prediction-accuracy input.
        if let Some(predicted_us) = predicted_execution_time_us {
            BuilderMetrics::tx_predicted_execution_time_us().record(predicted_us as f64);
        }

        GateVerdict::Admit
    }
}

/// Rejects blob and deposit transactions, which must never be sourced from the pool.
#[derive(Debug, Clone, Copy)]
pub struct SequencerGate;

impl Gate for SequencerGate {
    fn evaluate(
        &self,
        candidate: &mut Candidate,
        _info: &ExecutionInfo,
        _limits: &ResourceLimits,
        _db: &mut impl Database,
    ) -> GateVerdict {
        if candidate.tx.is_eip4844() || candidate.tx.is_deposit() {
            return GateVerdict::Reject(GateRejection::Sequencer);
        }
        GateVerdict::Admit
    }
}
