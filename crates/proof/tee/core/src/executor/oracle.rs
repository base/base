use std::{collections::HashMap, fmt, sync::Arc};

use async_trait::async_trait;
use base_proof::FlushableCache;
use base_proof_preimage::{
    HintWriterClient, PreimageKey, PreimageOracleClient,
    errors::{PreimageOracleError, PreimageOracleResult},
};

/// HashMap-backed preimage oracle for in-enclave stateless execution.
///
/// Loaded once from the proposer's preimage bundle; all I/O is local
/// `HashMap` lookups. [`HintWriterClient`] and [`FlushableCache`] are no-ops
/// because data is pre-loaded and immutable.
#[derive(Clone)]
pub struct Oracle {
    preimages: Arc<HashMap<PreimageKey, Vec<u8>>>,
}

impl Oracle {
    /// Construct an [`Oracle`] from an iterator of `(key, value)` pairs.
    pub fn new(preimages: impl IntoIterator<Item = (PreimageKey, Vec<u8>)>) -> Self {
        Self { preimages: Arc::new(preimages.into_iter().collect()) }
    }
}

impl fmt::Debug for Oracle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let total_bytes: usize = self.preimages.values().map(Vec::len).sum();
        f.debug_struct("Oracle")
            .field("keys", &self.preimages.len())
            .field("total_bytes", &total_bytes)
            .finish()
    }
}

#[async_trait]
impl PreimageOracleClient for Oracle {
    async fn get(&self, key: PreimageKey) -> PreimageOracleResult<Vec<u8>> {
        self.preimages.get(&key).cloned().ok_or(PreimageOracleError::KeyNotFound)
    }

    async fn get_exact(&self, key: PreimageKey, buf: &mut [u8]) -> PreimageOracleResult<()> {
        let value = self.preimages.get(&key).ok_or(PreimageOracleError::KeyNotFound)?;
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
