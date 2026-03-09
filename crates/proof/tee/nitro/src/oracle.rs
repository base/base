use std::{collections::HashMap, fmt, sync::Arc};

use async_trait::async_trait;
use base_proof_preimage::{
    FlushableCache, HintWriterClient, PreimageKey, PreimageOracleClient, WitnessOracle,
    errors::{PreimageOracleError, PreimageOracleResult, WitnessOracleResult},
};
use parking_lot::RwLock;

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
    pub fn new(preimages: impl IntoIterator<Item = (PreimageKey, Vec<u8>)>) -> Self {
        Self { preimages: Arc::new(RwLock::new(preimages.into_iter().collect())) }
    }

    /// Construct an empty [`Oracle`] for witness capture.
    pub fn empty() -> Self {
        Self { preimages: Arc::new(RwLock::new(HashMap::new())) }
    }

    /// Consume the oracle and return all captured preimages.
    ///
    /// # Panics
    ///
    /// Panics if other references to the internal preimage map still exist.
    /// This is only valid after [`Host::build_witness`] has returned the owned
    /// oracle and all internal clones have been dropped.
    pub fn into_preimages(self) -> Vec<(PreimageKey, Vec<u8>)> {
        Arc::try_unwrap(self.preimages)
            .expect("Oracle still has outstanding Arc references")
            .into_inner()
            .into_iter()
            .collect()
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
    fn new_roundtrip() {
        let key = PreimageKey::new([1u8; 32], PreimageKeyType::Local);
        let value = vec![0xAB; 128];

        let oracle = Oracle::new(vec![(key, value.clone())]);
        let read = oracle.preimages.read();
        assert_eq!(read.get(&key).unwrap(), &value);
    }

    #[tokio::test]
    async fn get_returns_key_not_found() {
        let oracle = Oracle::empty();
        let key = PreimageKey::new([2u8; 32], PreimageKeyType::Local);
        let result = oracle.get(key).await;
        assert!(matches!(result, Err(PreimageOracleError::KeyNotFound)));
    }
}
