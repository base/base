//! Shared test utilities for the `base-proof-tee-nitro-host` crate.

use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

use alloy_primitives::Address;
use base_proof_contracts::TEEProverRegistryClient;
use jsonrpsee::core::async_trait;

/// In-memory mock of [`TEEProverRegistryClient`] for unit tests.
///
/// Tracks call counts and supports injecting failures via [`should_fail`](Self::should_fail).
#[derive(Debug, Clone)]
pub struct MockRegistry {
    /// Whether `is_valid_signer` returns `true`.
    pub valid: Arc<AtomicBool>,
    /// Number of times `is_valid_signer` has been called.
    pub call_count: Arc<AtomicUsize>,
    /// When `true`, `is_valid_signer` returns a [`ContractError::Validation`] error.
    pub should_fail: Arc<AtomicBool>,
}

impl MockRegistry {
    /// Creates a new mock with the given initial validity and zero call count.
    pub fn new(valid: bool) -> Self {
        Self {
            valid: Arc::new(AtomicBool::new(valid)),
            call_count: Arc::new(AtomicUsize::new(0)),
            should_fail: Arc::new(AtomicBool::new(false)),
        }
    }
}

#[async_trait]
impl TEEProverRegistryClient for MockRegistry {
    async fn is_valid_signer(
        &self,
        _signer: Address,
    ) -> Result<bool, base_proof_contracts::ContractError> {
        self.call_count.fetch_add(1, Ordering::Relaxed);
        if self.should_fail.load(Ordering::Relaxed) {
            return Err(base_proof_contracts::ContractError::Validation("mock RPC failure".into()));
        }
        Ok(self.valid.load(Ordering::Relaxed))
    }

    async fn is_registered_signer(
        &self,
        _signer: Address,
    ) -> Result<bool, base_proof_contracts::ContractError> {
        unimplemented!()
    }

    async fn get_registered_signers(
        &self,
    ) -> Result<Vec<Address>, base_proof_contracts::ContractError> {
        unimplemented!()
    }
}
