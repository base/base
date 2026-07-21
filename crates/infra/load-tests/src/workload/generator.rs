use std::sync::Arc;

use alloy_rpc_types::TransactionRequest;
use tracing::instrument;

use super::{Payload, SeededRng};
use crate::{BaselineError, config::WorkloadConfig, utils::Result};

/// Selected payload plus whether it consumes the runner-supplied recipient.
#[derive(Debug, Clone)]
pub(crate) struct SelectedPayload {
    payload: Arc<dyn Payload>,
}

impl SelectedPayload {
    /// Returns true when this payload uses the runner-supplied recipient address.
    pub(crate) fn uses_runner_recipient(&self) -> bool {
        self.payload.uses_runner_recipient()
    }
}

/// Generates transaction workloads from configured payloads.
pub struct WorkloadGenerator {
    config: WorkloadConfig,
    rng: SeededRng,
    payloads: Vec<(Arc<dyn Payload>, f64)>,
}

impl WorkloadGenerator {
    /// Creates a new workload generator.
    pub fn new(config: WorkloadConfig) -> Self {
        let seed = config.seed.unwrap_or(0);
        Self { config, rng: SeededRng::new(seed), payloads: Vec::new() }
    }

    /// Adds a payload type to the generator.
    pub fn with_payload(mut self, payload: impl Payload + 'static, share_pct: f64) -> Self {
        self.payloads.push((Arc::new(payload), share_pct));
        self
    }

    /// Returns the workload configuration.
    pub const fn config(&self) -> &WorkloadConfig {
        &self.config
    }

    /// Generates a transaction payload with caller-provided addresses.
    #[instrument(skip(self))]
    pub fn generate_payload(
        &mut self,
        from: alloy_primitives::Address,
        to: alloy_primitives::Address,
    ) -> Result<TransactionRequest> {
        let payload = self.select_payload()?;
        Ok(self.generate_selected_payload(&payload, from, to))
    }

    /// Selects a payload according to configured weights.
    pub(crate) fn select_payload(&mut self) -> Result<SelectedPayload> {
        if self.payloads.is_empty() {
            return Err(BaselineError::Workload("no payloads configured".into()));
        }

        let total: f64 = self.payloads.iter().map(|(_, share)| share).sum();
        let mut target: f64 = self.rng.gen_range(0.0..total);

        for (payload, share) in &self.payloads {
            target -= share;
            if target <= 0.0 {
                return Ok(SelectedPayload { payload: Arc::clone(payload) });
            }
        }

        Ok(SelectedPayload {
            payload: Arc::clone(&self.payloads.last().expect("non-empty checked above").0),
        })
    }

    /// Generates a transaction request for a preselected payload.
    pub(crate) fn generate_selected_payload(
        &mut self,
        selected: &SelectedPayload,
        from: alloy_primitives::Address,
        to: alloy_primitives::Address,
    ) -> TransactionRequest {
        selected.payload.generate(&mut self.rng, from, to)
    }

    /// Resets the generator to its initial state.
    pub fn reset(&mut self) {
        let seed = self.config.seed.unwrap_or(0);
        self.rng = SeededRng::new(seed);
    }
}

impl std::fmt::Debug for WorkloadGenerator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkloadGenerator")
            .field("config", &self.config)
            .field("payloads_count", &self.payloads.len())
            .finish()
    }
}
