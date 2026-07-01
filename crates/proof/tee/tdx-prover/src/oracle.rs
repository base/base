//! HashMap-backed preimage oracle for local TDX proof execution.

use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};

use async_trait::async_trait;
use base_proof_preimage::{
    FlushableCache, HintWriterClient, PreimageKey, PreimageOracleClient, WitnessOracle,
    errors::{PreimageOracleError, PreimageOracleResult, WitnessOracleResult},
};

/// HashMap-backed preimage oracle for TDX proof execution.
#[derive(Clone, Debug)]
pub struct Oracle {
    preimages: Arc<RwLock<HashMap<PreimageKey, Vec<u8>>>>,
}

impl Oracle {
    /// Construct an empty [`Oracle`] for witness capture.
    pub fn empty() -> Self {
        Self { preimages: Arc::new(RwLock::new(HashMap::new())) }
    }
}

#[async_trait]
impl PreimageOracleClient for Oracle {
    async fn get(&self, key: PreimageKey) -> PreimageOracleResult<Vec<u8>> {
        self.preimages
            .read()
            .expect("oracle lock poisoned")
            .get(&key)
            .cloned()
            .ok_or(PreimageOracleError::KeyNotFound)
    }

    async fn get_exact(&self, key: PreimageKey, buf: &mut [u8]) -> PreimageOracleResult<()> {
        let value = self.get(key).await?;
        if value.len() != buf.len() {
            return Err(PreimageOracleError::BufferLengthMismatch(buf.len(), value.len()));
        }
        buf.copy_from_slice(&value);
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
        self.preimages.write().expect("oracle lock poisoned").insert(key, value.to_vec());
        Ok(())
    }

    fn finalize(&self) -> WitnessOracleResult<()> {
        Ok(())
    }

    fn preimage_count(&self) -> WitnessOracleResult<usize> {
        Ok(self.preimages.read().expect("oracle lock poisoned").len())
    }
}
