use crate::BytesHasherBuilder;
use alloy_primitives::{keccak256, FixedBytes};
use anyhow::Result;
use anyhow::{anyhow, Result as AnyhowResult};
use async_trait::async_trait;
use itertools::Itertools;
use kona_preimage::{
    errors::{PreimageOracleError, PreimageOracleResult},
    HintWriterClient, PreimageKey, PreimageKeyType, PreimageOracleClient,
};
use kona_proof::FlushableCache;
use kzg_rs::{get_kzg_settings, Blob as KzgRsBlob, Bytes48};
use rkyv::{from_bytes, Archive};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use tracing::info;

use super::StoreOracle;

/// An in-memory HashMap that will serve as the oracle for the zkVM.
/// Rather than relying on a trusted host for data, the data in this oracle
/// is verified with the `verify()` function, and then is trusted for
/// the remainder of execution.
///
/// Note: [u8; 32] is the key type, as serializing and deserializing PreimageKey is expensive.
#[derive(
    Debug, Clone, serde::Serialize, serde::Deserialize, Archive, rkyv::Serialize, rkyv::Deserialize,
)]
pub struct InMemoryOracle {
    pub cache: HashMap<[u8; 32], Vec<u8>, BytesHasherBuilder>,
}

impl InMemoryOracle {
    /// Creates a new [InMemoryOracle] from the raw bytes passed into the zkVM.
    /// These values are deserialized using rkyv for zero copy deserialization.
    pub fn from_raw_bytes(input: Vec<u8>) -> Self {
        println!("cycle-tracker-start: in-memory-oracle-from-raw-bytes-deserialize");
        let deserialized: InMemoryOracle =
            from_bytes::<InMemoryOracle, rkyv::rancor::Error>(&input)
                .expect("failed to deserialize");
        println!("cycle-tracker-end: in-memory-oracle-from-raw-bytes-deserialize");

        deserialized
    }

    /// Populates the InMemoryOracle with data from a StoreOracle.
    pub fn populate_from_store<OR, HW>(store_oracle: &StoreOracle<OR, HW>) -> Result<Self>
    where
        OR: PreimageOracleClient,
        HW: HintWriterClient,
    {
        let mut cache: HashMap<[u8; 32], Vec<u8>, BytesHasherBuilder> =
            HashMap::with_hasher(BytesHasherBuilder);
        // Lock the cache for safe access
        let cache_guard = store_oracle.cache.lock();

        // Iterate over each key-value pair in the cache
        for (key, value) in cache_guard.iter() {
            let key_bytes: [u8; 32] = (*key).into();
            cache.insert(key_bytes, value.clone());
        }
        Ok(Self { cache })
    }
}

#[async_trait]
impl PreimageOracleClient for InMemoryOracle {
    async fn get(&self, key: PreimageKey) -> Result<Vec<u8>, PreimageOracleError> {
        let key_bytes: [u8; 32] = key.into();
        self.cache
            .get(&key_bytes)
            .cloned()
            .ok_or_else(|| PreimageOracleError::KeyNotFound)
    }

    async fn get_exact(&self, key: PreimageKey, buf: &mut [u8]) -> Result<(), PreimageOracleError> {
        let value = self.get(key).await?;
        buf.copy_from_slice(value.as_slice());
        Ok(())
    }
}

#[async_trait]
impl HintWriterClient for InMemoryOracle {
    async fn write(&self, _hint: &str) -> Result<(), PreimageOracleError> {
        Ok(())
    }
}

impl FlushableCache for InMemoryOracle {
    fn flush(&self) {}
}

/// A data structure representing a blob. This data is held in memory for future verification.
/// This is used so that we can aggregate all separate blob elements into a single blob
/// and verify it once, rather than verifying each of the 4096 elements separately.
#[derive(Default)]
struct Blob {
    _commitment: FixedBytes<48>,
    // 4096 Field elements, each 32 bytes.
    data: FixedBytes<131072>,
    kzg_proof: FixedBytes<48>,
}

pub fn verify_preimage(key: &PreimageKey, value: &[u8]) -> PreimageOracleResult<()> {
    let key_type = key.key_type();
    let preimage = match key_type {
        PreimageKeyType::Keccak256 => Some(keccak256(value).0),
        PreimageKeyType::Sha256 => Some(Sha256::digest(value).into()),
        PreimageKeyType::Precompile | PreimageKeyType::Blob => unimplemented!(),
        PreimageKeyType::Local | PreimageKeyType::GlobalGeneric => None,
    };

    if let Some(preimage) = preimage {
        if key != &PreimageKey::new(preimage, key_type) {
            return Err(PreimageOracleError::InvalidPreimageKey);
        }
    }

    Ok(())
}

impl InMemoryOracle {
    /// Verifies all data in the oracle. Once the function has been called, all data in the
    /// oracle can be trusted for the remainder of execution.
    ///
    /// TODO(r): Switch to using the BlobProvider to save the witness and verify this.
    pub fn verify(&self) -> AnyhowResult<()> {
        let mut blobs: HashMap<FixedBytes<48>, Blob, BytesHasherBuilder> =
            HashMap::with_hasher(BytesHasherBuilder);

        for (key, value) in self.cache.iter() {
            let preimage_key = PreimageKey::try_from(*key).unwrap();
            if preimage_key.key_type() == PreimageKeyType::Blob {
                // We should verify the keys using the Blob provider.
                let blob_data_key: [u8; 32] =
                    PreimageKey::new(*key, PreimageKeyType::Keccak256).into();
                if let Some(blob_data) = self.cache.get(&blob_data_key) {
                    let commitment: FixedBytes<48> = blob_data[..48].try_into().unwrap();
                    let element_idx_bytes: [u8; 8] = blob_data[72..].try_into().unwrap();
                    let element_idx: u64 = u64::from_be_bytes(element_idx_bytes);

                    // Blob is stored as one 48 byte element.
                    if element_idx == 4096 {
                        blobs
                            .entry(commitment)
                            .or_default()
                            .kzg_proof
                            .copy_from_slice(value);
                        continue;
                    }

                    // Add the 32 bytes of blob data into the correct spot in the blob.
                    blobs
                        .entry(commitment)
                        .or_default()
                        .data
                        .get_mut((element_idx as usize) << 5..(element_idx as usize + 1) << 5)
                        .map(|slice| {
                            if slice.iter().all(|&byte| byte == 0) {
                                slice.copy_from_slice(value);
                                Ok(())
                            } else {
                                Err(anyhow!("trying to overwrite existing blob data"))
                            }
                        });
                }
            } else {
                verify_preimage(&preimage_key, value)?;
            }
        }

        println!("cycle-tracker-report-start: blob-verification");
        let commitments: Vec<Bytes48> = blobs
            .keys()
            .cloned()
            .map(|blob| Bytes48::from_slice(&blob.0).unwrap())
            .collect_vec();
        let kzg_proofs: Vec<Bytes48> = blobs
            .values()
            .map(|blob| Bytes48::from_slice(&blob.kzg_proof.0).unwrap())
            .collect_vec();
        let blob_datas: Vec<KzgRsBlob> = blobs
            .values()
            .map(|blob| KzgRsBlob::from_slice(&blob.data.0).unwrap())
            .collect_vec();
        info!("Verifying {} blobs", blob_datas.len());
        // Verify reconstructed blobs.
        kzg_rs::KzgProof::verify_blob_kzg_proof_batch(
            blob_datas,
            commitments,
            kzg_proofs,
            &get_kzg_settings(),
        )
        .map_err(|e| anyhow!("blob verification failed for batch: {:?}", e))?;
        println!("cycle-tracker-report-end: blob-verification");

        Ok(())
    }
}
