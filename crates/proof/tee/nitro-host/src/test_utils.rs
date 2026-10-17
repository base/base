//! Shared test utilities for the `base-proof-tee-nitro-host` crate.
//!
//! Registry responses remain mutable across health, latch, and proving checks.

use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

use alloy_primitives::{Address, B256};
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
    /// Registered image hash returned for every signer.
    pub image_hash: Arc<Mutex<B256>>,
    /// Number of image hash reads.
    pub image_hash_call_count: Arc<AtomicUsize>,
    /// Whether image hash reads fail independently of validity reads.
    pub image_hash_should_fail: Arc<AtomicBool>,
    /// Whether image hash reads wait forever.
    pub image_hash_should_stall: Arc<AtomicBool>,
}

impl MockRegistry {
    /// Creates a new mock with the given initial validity and zero call count.
    pub fn new(valid: bool) -> Self {
        Self {
            valid: Arc::new(AtomicBool::new(valid)),
            call_count: Arc::new(AtomicUsize::new(0)),
            should_fail: Arc::new(AtomicBool::new(false)),
            image_hash: Arc::new(Mutex::new(B256::ZERO)),
            image_hash_call_count: Arc::new(AtomicUsize::new(0)),
            image_hash_should_fail: Arc::new(AtomicBool::new(false)),
            image_hash_should_stall: Arc::new(AtomicBool::new(false)),
        }
    }
}

#[async_trait]
impl TEEProverRegistryClient for MockRegistry {
    fn address(&self) -> Address {
        Address::ZERO
    }

    async fn nitro_validator(&self) -> Result<Address, base_proof_contracts::ContractError> {
        unimplemented!()
    }

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

    async fn signer_image_hash(
        &self,
        _signer: Address,
    ) -> Result<B256, base_proof_contracts::ContractError> {
        self.image_hash_call_count.fetch_add(1, Ordering::Relaxed);
        if self.image_hash_should_stall.load(Ordering::Relaxed) {
            std::future::pending::<()>().await;
        }
        if self.image_hash_should_fail.load(Ordering::Relaxed) {
            return Err(base_proof_contracts::ContractError::Validation(
                "mock image hash RPC failure".into(),
            ));
        }
        Ok(*self.image_hash.lock().unwrap())
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

/// Mock registry that returns different validity per signer address.
#[derive(Debug, Clone)]
pub struct AddressBasedMockRegistry {
    /// Per-address validity map; addresses not in the map default to `false`.
    pub validity_map: Arc<Mutex<HashMap<Address, bool>>>,
    /// Number of times `is_valid_signer` has been called.
    pub call_count: Arc<AtomicUsize>,
    /// When `true`, all validity reads return an error.
    pub should_fail: Arc<AtomicBool>,
    /// Per-address override; validity reads for any signer in this set return an error.
    pub fail_signers: Arc<Mutex<HashSet<Address>>>,
    /// Registered image hashes; missing entries read as zero.
    pub image_hashes: Arc<Mutex<HashMap<Address, B256>>>,
    /// Signers queried for their image hash, in call order.
    pub image_hash_calls: Arc<Mutex<Vec<Address>>>,
    /// Signers whose image hash reads fail independently of validity reads.
    pub image_hash_fail_signers: Arc<Mutex<HashSet<Address>>>,
}

impl AddressBasedMockRegistry {
    /// Creates a new mock with the given per-address validity map.
    pub fn new(validity_map: HashMap<Address, bool>) -> Self {
        Self {
            validity_map: Arc::new(Mutex::new(validity_map)),
            call_count: Arc::new(AtomicUsize::new(0)),
            should_fail: Arc::new(AtomicBool::new(false)),
            fail_signers: Arc::new(Mutex::new(HashSet::new())),
            image_hashes: Arc::new(Mutex::new(HashMap::new())),
            image_hash_calls: Arc::new(Mutex::new(Vec::new())),
            image_hash_fail_signers: Arc::new(Mutex::new(HashSet::new())),
        }
    }
}

#[async_trait]
impl TEEProverRegistryClient for AddressBasedMockRegistry {
    fn address(&self) -> Address {
        Address::ZERO
    }

    async fn nitro_validator(&self) -> Result<Address, base_proof_contracts::ContractError> {
        unimplemented!()
    }

    async fn is_valid_signer(
        &self,
        signer: Address,
    ) -> Result<bool, base_proof_contracts::ContractError> {
        self.call_count.fetch_add(1, Ordering::Relaxed);
        if self.should_fail.load(Ordering::Relaxed) {
            return Err(base_proof_contracts::ContractError::Validation("mock RPC failure".into()));
        }
        if self.fail_signers.lock().expect("fail_signers poisoned").contains(&signer) {
            return Err(base_proof_contracts::ContractError::Validation("mock RPC failure".into()));
        }
        let map = self.validity_map.lock().expect("validity map poisoned");
        Ok(map.get(&signer).copied().unwrap_or(false))
    }

    async fn signer_image_hash(
        &self,
        signer: Address,
    ) -> Result<B256, base_proof_contracts::ContractError> {
        self.image_hash_calls.lock().unwrap().push(signer);
        if self.image_hash_fail_signers.lock().unwrap().contains(&signer) {
            return Err(base_proof_contracts::ContractError::Validation(
                "mock image hash RPC failure".into(),
            ));
        }
        Ok(self.image_hashes.lock().unwrap().get(&signer).copied().unwrap_or_default())
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
