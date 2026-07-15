//! Per-candidate build policy for the flashblock build loop.
//!
//! Extracts the resource metering that is otherwise interleaved into the build loop into a
//! standalone [`MeteringBuildPolicy`]. Given a candidate transaction, the policy consults the
//! [`MeteringProvider`](crate::MeteringProvider) to decide whether the candidate must wait for
//! metering data, and otherwise produces the [`TxResources`] estimate that the loop's generic,
//! metering-agnostic limit check consumes. This is the default build behaviour; the loop stays
//! oblivious to how resources are estimated.

use core::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

use alloy_primitives::TxHash;
use base_bundles::MeterBundleResponse;

use crate::{BuilderMetrics, SharedMeteringProvider, TxResources};

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
    ) -> Self {
        Self {
            metering_provider,
            state_root_gas_coefficient,
            state_root_gas_anchor_us,
            metering_wait_duration,
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
            MeteringBuildPolicy::new(Arc::new(NoopMeteringProvider), 0.02, 5_000, None);

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
        let policy = MeteringBuildPolicy::new(Arc::new(provider), 0.02, 5_000, None);

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
        let policy = MeteringBuildPolicy::new(Arc::new(provider), 0.02, 5_000, None);

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
        );

        let estimate = policy.estimate_resources(TxHash::default(), 0, 21_000, 0, now_ms());

        assert!(matches!(estimate, MeteringEstimate::Ready(_)));
    }
}
