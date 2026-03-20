use std::sync::Arc;

use alloy_rpc_types::TransactionRequest;
use tracing::instrument;

use super::{Payload, SeededRng};
use crate::{BaselineError, config::WorkloadConfig, utils::Result};

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
        Ok(payload.generate(&mut self.rng, from, to))
    }

    fn select_payload(&mut self) -> Result<Arc<dyn Payload>> {
        if self.payloads.is_empty() {
            return Err(BaselineError::Workload("no payloads configured".into()));
        }

        let total: f64 = self.payloads.iter().map(|(_, share)| share).sum();
        let mut target: f64 = self.rng.gen_range(0.0..total);

        for (payload, share) in &self.payloads {
            target -= share;
            if target <= 0.0 {
                return Ok(Arc::clone(payload));
            }
        }

        Ok(Arc::clone(&self.payloads.last().expect("non-empty checked above").0))
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
