use std::sync::Arc;

use async_trait::async_trait;
use base_proof_preimage::{
    FlushableCache, HintWriterClient, PreimageKey, PreimageOracleClient, WitnessOracle,
    errors::{PreimageOracleError, PreimageOracleResult},
};

/// A transparent oracle wrapper that records every preimage fetched into a [`WitnessOracle`].
///
/// `RecordingOracle` sits between the client program and the real oracle backend,
/// intercepting every `get` call. It delegates to the inner oracle, then inserts the
/// returned preimage into the witness oracle so that all data required for later
/// offline replay is captured.
///
/// Hints are forwarded to the inner hint writer without recording — they trigger
/// fetches which come back through `get` and are recorded there.
#[derive(Debug)]
pub struct RecordingOracle<P, H, W> {
    /// The inner preimage oracle client (e.g. `OracleReader`).
    oracle: P,
    /// The inner hint writer client (e.g. `HintWriter`).
    hint: H,
    /// The witness oracle that accumulates recorded preimages.
    witness: Arc<W>,
}

impl<P, H, W> RecordingOracle<P, H, W> {
    /// Creates a new [`RecordingOracle`].
    pub const fn new(oracle: P, hint: H, witness: Arc<W>) -> Self {
        Self { oracle, hint, witness }
    }
}

impl<P: Clone, H: Clone, W> Clone for RecordingOracle<P, H, W> {
    fn clone(&self) -> Self {
        Self {
            oracle: self.oracle.clone(),
            hint: self.hint.clone(),
            witness: Arc::clone(&self.witness),
        }
    }
}

#[async_trait]
impl<P, H, W> PreimageOracleClient for RecordingOracle<P, H, W>
where
    P: PreimageOracleClient + Send + Sync,
    H: Send + Sync,
    W: WitnessOracle,
{
    async fn get(&self, key: PreimageKey) -> PreimageOracleResult<Vec<u8>> {
        let value = self.oracle.get(key).await?;
        self.witness
            .insert_preimage(key, &value)
            .map_err(|e| PreimageOracleError::Other(e.to_string()))?;
        Ok(value)
    }

    async fn get_exact(&self, key: PreimageKey, buf: &mut [u8]) -> PreimageOracleResult<()> {
        self.oracle.get_exact(key, buf).await?;
        self.witness
            .insert_preimage(key, buf)
            .map_err(|e| PreimageOracleError::Other(e.to_string()))?;
        Ok(())
    }
}

#[async_trait]
impl<P, H, W> HintWriterClient for RecordingOracle<P, H, W>
where
    P: Send + Sync,
    H: HintWriterClient + Send + Sync,
    W: Send + Sync,
{
    async fn write(&self, hint: &str) -> PreimageOracleResult<()> {
        self.hint.write(hint).await
    }
}

impl<P, H, W> FlushableCache for RecordingOracle<P, H, W>
where
    P: FlushableCache,
{
    fn flush(&self) {
        self.oracle.flush();
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Mutex};

    use base_proof_preimage::{PreimageKeyType, WitnessOracleResult, errors::PreimageOracleError};

    use super::*;

    #[derive(Debug, Clone)]
    struct MockOracle {
        data: HashMap<PreimageKey, Vec<u8>>,
    }

    #[async_trait]
    impl PreimageOracleClient for MockOracle {
        async fn get(&self, key: PreimageKey) -> PreimageOracleResult<Vec<u8>> {
            Ok(self.data.get(&key).cloned().unwrap_or_default())
        }

        async fn get_exact(&self, key: PreimageKey, buf: &mut [u8]) -> PreimageOracleResult<()> {
            if let Some(data) = self.data.get(&key) {
                let len = buf.len().min(data.len());
                buf[..len].copy_from_slice(&data[..len]);
            }
            Ok(())
        }
    }

    #[derive(Debug, Clone, Default)]
    struct MockHintWriter {
        hints: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl HintWriterClient for MockHintWriter {
        async fn write(&self, hint: &str) -> PreimageOracleResult<()> {
            self.hints.lock().unwrap().push(hint.to_string());
            Ok(())
        }
    }

    #[allow(clippy::type_complexity)]
    #[derive(Debug, Clone, Default)]
    struct MockWitness {
        preimages: Arc<Mutex<Vec<(PreimageKey, Vec<u8>)>>>,
    }

    impl WitnessOracle for MockWitness {
        fn insert_preimage(&self, key: PreimageKey, value: &[u8]) -> WitnessOracleResult<()> {
            self.preimages.lock().unwrap().push((key, value.to_vec()));
            Ok(())
        }

        fn finalize(&self) -> WitnessOracleResult<()> {
            Ok(())
        }

        fn preimage_count(&self) -> WitnessOracleResult<usize> {
            Ok(self.preimages.lock().unwrap().len())
        }
    }

    #[tokio::test]
    async fn test_get_records_preimage() {
        let key = PreimageKey::new([1u8; 32], PreimageKeyType::Keccak256);
        let value = vec![0xDE, 0xAD, 0xBE, 0xEF];

        let oracle = MockOracle { data: HashMap::from([(key, value.clone())]) };
        let hint = MockHintWriter::default();
        let witness = Arc::new(MockWitness::default());

        let recording = RecordingOracle::new(oracle, hint, Arc::clone(&witness));

        let result = recording.get(key).await.unwrap();
        assert_eq!(result, value);

        let recorded = witness.preimages.lock().unwrap();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0], (key, value));
    }

    #[tokio::test]
    async fn test_get_exact_records_preimage() {
        let key = PreimageKey::new([2u8; 32], PreimageKeyType::Keccak256);
        let value = vec![0xCA, 0xFE];

        let oracle = MockOracle { data: HashMap::from([(key, value.clone())]) };
        let hint = MockHintWriter::default();
        let witness = Arc::new(MockWitness::default());

        let recording = RecordingOracle::new(oracle, hint, Arc::clone(&witness));

        let mut buf = vec![0u8; 2];
        recording.get_exact(key, &mut buf).await.unwrap();
        assert_eq!(buf, value);

        let recorded = witness.preimages.lock().unwrap();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0], (key, value));
    }

    #[tokio::test]
    async fn test_hint_forwarded_not_recorded() {
        let oracle = MockOracle { data: HashMap::new() };
        let hint = MockHintWriter::default();
        let witness = Arc::new(MockWitness::default());

        let recording = RecordingOracle::new(oracle, hint.clone(), Arc::clone(&witness));

        recording.write("test-hint").await.unwrap();

        let hints = hint.hints.lock().unwrap();
        assert_eq!(hints.len(), 1);
        assert_eq!(hints[0], "test-hint");

        assert_eq!(witness.preimage_count().unwrap(), 0);
    }

    #[tokio::test]
    async fn test_flush_delegates_to_inner() {
        #[derive(Debug, Clone)]
        struct FlushTracker {
            flushed: Arc<Mutex<bool>>,
        }

        #[async_trait]
        impl PreimageOracleClient for FlushTracker {
            async fn get(&self, _key: PreimageKey) -> PreimageOracleResult<Vec<u8>> {
                Ok(Vec::new())
            }

            async fn get_exact(
                &self,
                _key: PreimageKey,
                _buf: &mut [u8],
            ) -> PreimageOracleResult<()> {
                Ok(())
            }
        }

        impl FlushableCache for FlushTracker {
            fn flush(&self) {
                *self.flushed.lock().unwrap() = true;
            }
        }

        let flushed = Arc::new(Mutex::new(false));
        let oracle = FlushTracker { flushed: Arc::clone(&flushed) };
        let hint = MockHintWriter::default();
        let witness = Arc::new(MockWitness::default());

        let recording = RecordingOracle::new(oracle, hint, witness);
        recording.flush();

        assert!(*flushed.lock().unwrap());
    }

    #[tokio::test]
    async fn test_get_error_does_not_record() {
        let key = PreimageKey::new([4u8; 32], PreimageKeyType::Keccak256);

        #[derive(Debug, Clone)]
        struct FailingOracle;

        #[async_trait]
        impl PreimageOracleClient for FailingOracle {
            async fn get(&self, _key: PreimageKey) -> PreimageOracleResult<Vec<u8>> {
                Err(PreimageOracleError::KeyNotFound)
            }

            async fn get_exact(
                &self,
                _key: PreimageKey,
                _buf: &mut [u8],
            ) -> PreimageOracleResult<()> {
                Err(PreimageOracleError::KeyNotFound)
            }
        }

        let oracle = FailingOracle;
        let hint = MockHintWriter::default();
        let witness = Arc::new(MockWitness::default());

        let recording = RecordingOracle::new(oracle, hint, Arc::clone(&witness));

        assert!(recording.get(key).await.is_err());
        assert_eq!(witness.preimage_count().unwrap(), 0, "get error should not record into witness");

        let mut buf = vec![0u8; 4];
        assert!(recording.get_exact(key, &mut buf).await.is_err());
        assert_eq!(
            witness.preimage_count().unwrap(),
            0,
            "get_exact error should not record into witness"
        );
    }

    #[tokio::test]
    async fn test_clone_shares_witness() {
        let key = PreimageKey::new([3u8; 32], PreimageKeyType::Keccak256);
        let value = vec![0x01];

        let oracle = MockOracle { data: HashMap::from([(key, value.clone())]) };
        let hint = MockHintWriter::default();
        let witness = Arc::new(MockWitness::default());

        let recording = RecordingOracle::new(oracle, hint, Arc::clone(&witness));
        let cloned = recording.clone();

        recording.get(key).await.unwrap();
        cloned.get(key).await.unwrap();

        assert_eq!(witness.preimage_count().unwrap(), 2);
    }
}
