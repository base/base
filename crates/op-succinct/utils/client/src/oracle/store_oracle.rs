//! Contains the [CachingOracle], which is a wrapper around an [OracleReader] and [HintWriter] that
//! stores a configurable number of responses in an [LruCache] for quick retrieval.
//!
//! [OracleReader]: kona_preimage::OracleReader
//! [HintWriter]: kona_preimage::HintWriter

use alloc::{boxed::Box, sync::Arc, vec::Vec};
use async_trait::async_trait;
use kona_preimage::{
    errors::PreimageOracleResult, HintWriterClient, PreimageKey, PreimageOracleClient,
};
use kona_proof::FlushableCache;
use spin::Mutex;
use std::collections::HashMap;

/// A wrapper around an [OracleReader] and [HintWriter] that stores a configurable number of
/// responses in an [LruCache] for quick retrieval.
///
/// [OracleReader]: kona_preimage::OracleReader
/// [HintWriter]: kona_preimage::HintWriter
#[allow(unreachable_pub)]
#[derive(Debug, Clone)]
pub struct StoreOracle<OR, HW>
where
    OR: PreimageOracleClient,
    HW: HintWriterClient,
{
    /// The spin-locked cache that stores the responses from the oracle.
    pub cache: Arc<Mutex<HashMap<PreimageKey, Vec<u8>>>>,
    /// Oracle reader type.
    oracle_reader: OR,
    /// Hint writer type.
    hint_writer: HW,
}

impl<OR, HW> StoreOracle<OR, HW>
where
    OR: PreimageOracleClient,
    HW: HintWriterClient,
{
    /// Creates a new [CachingOracle] that wraps the given [OracleReader] and stores up to `N`
    /// responses in the cache.
    ///
    /// [OracleReader]: kona_preimage::OracleReader
    pub fn new(oracle_reader: OR, hint_writer: HW) -> Self {
        Self { cache: Arc::new(Mutex::new(HashMap::new())), oracle_reader, hint_writer }
    }
}

impl<OR, HW> FlushableCache for StoreOracle<OR, HW>
where
    OR: PreimageOracleClient,
    HW: HintWriterClient,
{
    /// Flushes the cache, removing all entries.
    fn flush(&self) {
        self.cache.lock().clear();
    }
}

#[async_trait]
impl<OR, HW> PreimageOracleClient for StoreOracle<OR, HW>
where
    OR: PreimageOracleClient + Sync,
    HW: HintWriterClient + Sync,
{
    async fn get(&self, key: PreimageKey) -> PreimageOracleResult<Vec<u8>> {
        let mut cache_lock = self.cache.lock();
        if let Some(value) = cache_lock.get(&key) {
            Ok(value.clone())
        } else {
            let value = self.oracle_reader.get(key).await?;
            cache_lock.insert(key, value.clone());
            Ok(value)
        }
    }

    async fn get_exact(&self, key: PreimageKey, buf: &mut [u8]) -> PreimageOracleResult<()> {
        let mut cache_lock = self.cache.lock();
        if let Some(value) = cache_lock.get(&key) {
            // SAFETY: The value never enters the cache unless the preimage length matches the
            // buffer length, due to the checks in the OracleReader.
            buf.copy_from_slice(value.as_slice());
            Ok(())
        } else {
            self.oracle_reader.get_exact(key, buf).await?;
            cache_lock.insert(key, buf.to_vec());
            Ok(())
        }
    }
}

#[async_trait]
impl<OR, HW> HintWriterClient for StoreOracle<OR, HW>
where
    OR: PreimageOracleClient + Sync,
    HW: HintWriterClient + Sync,
{
    async fn write(&self, hint: &str) -> PreimageOracleResult<()> {
        self.hint_writer.write(hint).await
    }
}
