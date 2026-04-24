use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use base_proof_preimage::{
    CommsClient, FlushableCache, HintWriterClient, PreimageKey, PreimageOracleClient,
    errors::{PreimageOracleError, PreimageOracleResult},
};
use base_proof_succinct_client_utils::witness::preimage_store::PreimageStore;

/// Wraps a preimage oracle and records all fetched preimages for witness generation.
#[derive(Clone, Debug)]
pub struct PreimageWitnessCollector<P: CommsClient + FlushableCache + Send + Sync + Clone> {
    /// Inner preimage oracle.
    pub preimage_oracle: Arc<P>,
    /// Store for collected preimage witness data.
    pub preimage_witness_store: Arc<Mutex<PreimageStore>>,
}

#[async_trait]
impl<P> PreimageOracleClient for PreimageWitnessCollector<P>
where
    P: CommsClient + FlushableCache + Send + Sync + Clone,
{
    async fn get(&self, key: PreimageKey) -> PreimageOracleResult<Vec<u8>> {
        let value = self.preimage_oracle.get(key).await?;
        self.save(key, &value)?;
        Ok(value)
    }

    async fn get_exact(&self, key: PreimageKey, buf: &mut [u8]) -> PreimageOracleResult<()> {
        self.preimage_oracle.get_exact(key, buf).await?;
        self.save(key, buf)?;
        Ok(())
    }
}

#[async_trait]
impl<P> HintWriterClient for PreimageWitnessCollector<P>
where
    P: CommsClient + FlushableCache + Send + Sync + Clone,
{
    async fn write(&self, hint: &str) -> PreimageOracleResult<()> {
        self.preimage_oracle.write(hint).await
    }
}

impl<P> FlushableCache for PreimageWitnessCollector<P>
where
    P: CommsClient + FlushableCache + Send + Sync + Clone,
{
    fn flush(&self) {
        self.preimage_oracle.flush();
    }
}

impl<P> PreimageWitnessCollector<P>
where
    P: CommsClient + FlushableCache + Send + Sync + Clone,
{
    /// Persist a preimage key-value pair to the witness store.
    pub fn save(&self, key: PreimageKey, value: &[u8]) -> PreimageOracleResult<()> {
        let mut witness_store_lock = self.preimage_witness_store.lock().map_err(|_| {
            PreimageOracleError::Other("Failed to acquire preimage_witness_store lock".to_string())
        })?;
        witness_store_lock.save_preimage(key, value.to_vec())
    }
}
