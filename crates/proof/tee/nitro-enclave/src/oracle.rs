//! HashMap-backed preimage oracle for in-enclave stateless execution.

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

use crate::NitroError;

/// HashMap-backed preimage oracle for in-enclave stateless execution.
///
/// Stores preimages in a shared, mutable map so the same oracle can serve
/// both roles: **writing** (during witness capture via [`WitnessOracle`]) and
/// **reading** (during block re-execution via [`PreimageOracleClient`]).
///
/// [`HintWriterClient`] and [`FlushableCache`] are no-ops because TEE
/// execution doesn't route hints or manage an external cache.
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
    pub fn new(preimages: impl IntoIterator<Item = (PreimageKey, Vec<u8>)>) -> crate::Result<Self> {
        let map: HashMap<PreimageKey, Vec<u8>> = preimages.into_iter().collect();
        Self::check_preimages(&map)?;
        Ok(Self { preimages: Arc::new(RwLock::new(map)) })
    }

    /// Construct an empty [`Oracle`] for witness capture.
    pub fn empty() -> Self {
        Self { preimages: Arc::new(RwLock::new(HashMap::new())) }
    }

    /// Verify that a preimage's content matches its key for hash-based key types.
    ///
    /// For [`PreimageKeyType::Keccak256`] keys the keccak256 digest of `value`
    /// must produce a key equal to `key`. For [`PreimageKeyType::Sha256`] keys
    /// the SHA-256 digest is checked instead. Local and `GlobalGeneric` keys are
    /// context-dependent and cannot be verified by hash, so they are accepted
    /// without validation.
    ///
    /// Blob and Precompile keys use composite hashing schemes that cannot be
    /// validated with the value alone, so they are also accepted as-is.
    fn check_preimage(key: &PreimageKey, value: &[u8]) -> crate::Result<()> {
        let expected_hash: Option<[u8; 32]> = match key.key_type() {
            PreimageKeyType::Keccak256 => Some(keccak256(value).0),
            PreimageKeyType::Sha256 => Some(sha2::Sha256::digest(value).into()),
            // Blob keys are `keccak256(commitment ++ z)` and precompile keys are
            // `keccak256(address ++ input)` — neither can be re-derived from the
            // stored value alone, so we skip verifying them here and instead verify them
            // during derivation.
            PreimageKeyType::Local
            | PreimageKeyType::GlobalGeneric
            | PreimageKeyType::Blob
            | PreimageKeyType::Precompile => None,
        };

        if let Some(hash) = expected_hash
            && key != &PreimageKey::new(hash, key.key_type())
        {
            return Err(NitroError::InvalidPreimage(*key));
        }
        Ok(())
    }

    fn check_preimages(preimages: &HashMap<PreimageKey, Vec<u8>>) -> crate::Result<()> {
        for (key, value) in preimages {
            Self::check_preimage(key, value)?;
        }
        Ok(())
    }

    /// Consume the oracle and return all captured preimages.
    ///
    /// Returns an error if other references to the internal preimage map still
    /// exist. This is only valid after [`Host::build_witness`] has returned the
    /// owned oracle and all internal clones have been dropped.
    pub fn into_preimages(self) -> crate::Result<Vec<(PreimageKey, Vec<u8>)>> {
        Arc::try_unwrap(self.preimages)
            .map(|lock| lock.into_inner().into_iter().collect())
            .map_err(|_| NitroError::Internal("oracle still has outstanding references".into()))
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
    use alloy_primitives::keccak256;
    use base_proof_preimage::PreimageKeyType;
    use sha2::Digest;

    use super::*;

    #[test]
    fn new_roundtrip_local() {
        let key = PreimageKey::new([1u8; 32], PreimageKeyType::Local);
        let value = vec![0xAB; 128];

        let oracle = Oracle::new(vec![(key, value.clone())]).unwrap();
        let read = oracle.preimages.read();
        assert_eq!(read.get(&key).unwrap(), &value);
    }

    #[test]
    fn new_accepts_valid_keccak256() {
        let value = b"hello world";
        let digest = keccak256(value).0;
        let key = PreimageKey::new(digest, PreimageKeyType::Keccak256);

        let oracle = Oracle::new(vec![(key, value.to_vec())]);
        assert!(oracle.is_ok());
    }

    #[test]
    fn new_accepts_valid_sha256() {
        let value = b"hello world";
        let digest: [u8; 32] = sha2::Sha256::digest(value).into();
        let key = PreimageKey::new(digest, PreimageKeyType::Sha256);

        let oracle = Oracle::new(vec![(key, value.to_vec())]);
        assert!(oracle.is_ok());
    }

    #[test]
    fn new_rejects_invalid_keccak256() {
        let value = b"hello world";
        let wrong_key = PreimageKey::new([0xAA; 32], PreimageKeyType::Keccak256);

        let result = Oracle::new(vec![(wrong_key, value.to_vec())]);
        assert!(matches!(result, Err(NitroError::InvalidPreimage(_))));
    }

    #[test]
    fn new_rejects_invalid_sha256() {
        let value = b"hello world";
        let wrong_key = PreimageKey::new([0xBB; 32], PreimageKeyType::Sha256);

        let result = Oracle::new(vec![(wrong_key, value.to_vec())]);
        assert!(matches!(result, Err(NitroError::InvalidPreimage(_))));
    }

    #[test]
    fn new_accepts_local_without_hash_check() {
        let key = PreimageKey::new([0xFF; 32], PreimageKeyType::Local);
        let value = vec![0xDE, 0xAD];

        assert!(Oracle::new(vec![(key, value)]).is_ok());
    }

    #[test]
    fn new_accepts_global_generic_without_hash_check() {
        let key = PreimageKey::new([0xFF; 32], PreimageKeyType::GlobalGeneric);
        let value = vec![0xBE, 0xEF];

        assert!(Oracle::new(vec![(key, value)]).is_ok());
    }

    #[tokio::test]
    async fn get_returns_key_not_found() {
        let oracle = Oracle::empty();
        let key = PreimageKey::new([2u8; 32], PreimageKeyType::Local);
        let result = oracle.get(key).await;
        assert!(matches!(result, Err(PreimageOracleError::KeyNotFound)));
    }
}
