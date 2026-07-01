//! HashMap-backed preimage oracle for local TDX proof execution.

use std::{collections::HashMap, fmt, sync::Arc};

use alloy_primitives::keccak256;
use async_trait::async_trait;
use base_proof_preimage::{
    FlushableCache, HintWriterClient, PreimageKey, PreimageKeyType, PreimageOracleClient,
    WitnessOracle,
    errors::{PreimageOracleError, PreimageOracleResult, WitnessOracleResult},
};
use parking_lot::RwLock;
use sha2::Digest;

use crate::{Result, TdxProverError};

/// HashMap-backed preimage oracle for TDX proof execution.
#[derive(Clone)]
pub struct Oracle {
    preimages: Arc<RwLock<HashMap<PreimageKey, Vec<u8>>>>,
}

impl Oracle {
    /// Construct an [`Oracle`] from an iterator of `(key, value)` pairs.
    ///
    /// Every preimage with a hash-based key type (Keccak256, Sha256) is verified
    /// against its key before being accepted. Returns an error if any preimage
    /// fails validation.
    pub fn new(preimages: impl IntoIterator<Item = (PreimageKey, Vec<u8>)>) -> Result<Self> {
        let map: HashMap<PreimageKey, Vec<u8>> = preimages.into_iter().collect();
        for (key, value) in &map {
            Self::check_preimage(key, value)?;
        }
        Ok(Self { preimages: Arc::new(RwLock::new(map)) })
    }

    /// Construct an empty [`Oracle`] for witness capture.
    pub fn empty() -> Self {
        Self { preimages: Arc::new(RwLock::new(HashMap::new())) }
    }

    /// Verify that a preimage's content matches its key for hash-based key types.
    ///
    /// Blob and Precompile keys use composite hashing schemes (`keccak256(commitment ++ z)`
    /// and `keccak256(addr ++ input)` respectively) that cannot be re-derived from the
    /// stored value alone, so they are accepted as-is and validated during derivation.
    /// Local and `GlobalGeneric` keys are context-dependent and likewise skip validation.
    fn check_preimage(key: &PreimageKey, value: &[u8]) -> Result<()> {
        let computed: [u8; 32] = match key.key_type() {
            PreimageKeyType::Keccak256 => keccak256(value).0,
            PreimageKeyType::Sha256 => sha2::Sha256::digest(value).into(),
            PreimageKeyType::Local
            | PreimageKeyType::GlobalGeneric
            | PreimageKeyType::Blob
            | PreimageKeyType::Precompile => return Ok(()),
        };

        if key != &PreimageKey::new(computed, key.key_type()) {
            return Err(TdxProverError::InvalidPreimage(*key));
        }
        Ok(())
    }
}

impl fmt::Debug for Oracle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let preimages = self.preimages.read();
        let total_bytes: usize = preimages.values().map(Vec::len).sum();
        f.debug_struct("Oracle")
            .field("keys", &preimages.len())
            .field("total_bytes", &total_bytes)
            .finish()
    }
}

#[async_trait]
impl PreimageOracleClient for Oracle {
    async fn get(&self, key: PreimageKey) -> PreimageOracleResult<Vec<u8>> {
        self.preimages.read().get(&key).cloned().ok_or(PreimageOracleError::KeyNotFound)
    }

    async fn get_exact(&self, key: PreimageKey, buf: &mut [u8]) -> PreimageOracleResult<()> {
        let preimages = self.preimages.read();
        let value = preimages.get(&key).ok_or(PreimageOracleError::KeyNotFound)?;
        if value.len() != buf.len() {
            return Err(PreimageOracleError::BufferLengthMismatch(buf.len(), value.len()));
        }
        buf.copy_from_slice(value);
        Ok(())
    }
}

#[async_trait]
impl HintWriterClient for Oracle {
    async fn write(&self, _hint: &str) -> PreimageOracleResult<()> {
        Ok(())
    }
}

impl FlushableCache for Oracle {
    fn flush(&self) {}
}

impl WitnessOracle for Oracle {
    fn insert_preimage(&self, key: PreimageKey, value: &[u8]) -> WitnessOracleResult<()> {
        self.preimages.write().insert(key, value.to_vec());
        Ok(())
    }

    fn finalize(&self) -> WitnessOracleResult<()> {
        Ok(())
    }

    fn preimage_count(&self) -> WitnessOracleResult<usize> {
        Ok(self.preimages.read().len())
    }
}

#[cfg(test)]
mod tests {
    use base_proof_preimage::PreimageKeyType;

    use super::*;

    #[test]
    fn new_accepts_valid_keccak256() {
        let value = b"hello world";
        let digest = keccak256(value).0;
        let key = PreimageKey::new(digest, PreimageKeyType::Keccak256);

        let oracle = Oracle::new(vec![(key, value.to_vec())]);
        assert!(oracle.is_ok());
    }

    #[test]
    fn new_rejects_invalid_keccak256() {
        let value = b"hello world";
        let wrong_key = PreimageKey::new([0xAA; 32], PreimageKeyType::Keccak256);

        let result = Oracle::new(vec![(wrong_key, value.to_vec())]);
        assert!(matches!(result, Err(TdxProverError::InvalidPreimage(_))));
    }
}
