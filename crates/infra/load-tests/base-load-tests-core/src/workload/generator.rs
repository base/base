use std::sync::Arc;

use tracing::{debug, instrument};

use super::{AccountPool, Payload, SeededRng};
use crate::{BaselineError, config::WorkloadConfig, rpc::TransactionRequest, utils::Result};

/// Generates transaction workloads from configured payloads.
pub struct WorkloadGenerator {
    config: WorkloadConfig,
    rng: SeededRng,
    accounts: AccountPool,
    payloads: Vec<(Arc<dyn Payload>, f64)>,
}

impl WorkloadGenerator {
    /// Creates a new workload generator.
    pub fn new(config: WorkloadConfig, accounts: AccountPool) -> Self {
        let seed = config.seed.unwrap_or(0);
        Self { config, rng: SeededRng::new(seed), accounts, payloads: Vec::new() }
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

    /// Returns the account pool.
    pub const fn accounts(&self) -> &AccountPool {
        &self.accounts
    }

    /// Returns a mutable reference to the account pool.
    pub const fn accounts_mut(&mut self) -> &mut AccountPool {
        &mut self.accounts
    }

    /// Generates a batch of transactions.
    #[instrument(skip(self), fields(count = count))]
    pub fn generate_batch(&mut self, count: usize) -> Result<Vec<TransactionRequest>> {
        let mut requests = Vec::with_capacity(count);

        for _ in 0..count {
            let request = self.generate_single()?;
            requests.push(request);
        }

        debug!(generated = requests.len(), "generated transaction batch");
        Ok(requests)
    }

    /// Generates a single transaction payload with caller-provided addresses.
    pub fn generate_payload(
        &mut self,
        from: alloy_primitives::Address,
        to: alloy_primitives::Address,
    ) -> Result<TransactionRequest> {
        let payload = self.select_payload()?;
        Ok(payload.generate(&mut self.rng, from, to))
    }

    fn generate_single(&mut self) -> Result<TransactionRequest> {
        let payload = self.select_payload()?;
        let from_account = self.accounts.random_account();
        let from = from_account.address;

        let to_index = self.rng.gen_range(0..self.accounts.len());
        let to = self.accounts.accounts()[to_index].address;

        let request = payload.generate(&mut self.rng, from, to);

        Ok(request)
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

        Ok(Arc::clone(&self.payloads.last().unwrap().0))
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
            .field("accounts_count", &self.accounts.len())
            .field("payloads_count", &self.payloads.len())
            .finish()
    }
}
