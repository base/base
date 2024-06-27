//! Contains the host <-> client communication utilities.

mod precompile;
use precompile::Precompile;

use alloc::{boxed::Box, vec::Vec};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use kona_preimage::{HintWriterClient, PreimageKey, PreimageKeyType, PreimageOracleClient};
use std::collections::HashMap;
use sha2::{Digest, Sha256};
use rkyv::{Archive, Serialize, Deserialize, Infallible};
use zkvm_common::BytesHasherBuilder;
use alloy_primitives::{FixedBytes, keccak256};

#[derive(Debug, Clone, Archive, Serialize, Deserialize)]
pub struct InMemoryOracle {
    // TODO: Change this to PreimageKey and everything below.
    cache: HashMap<[u8;32], Vec<u8>, BytesHasherBuilder>,
}

impl InMemoryOracle {
    pub fn from_raw_bytes(input: Vec<u8>) -> Self {
        let archived = unsafe { rkyv::archived_root::<HashMap<[u8;32], Vec<u8>, BytesHasherBuilder>>(&input) };
        let deserialized: HashMap<[u8;32], Vec<u8>, BytesHasherBuilder> = archived.deserialize(&mut Infallible).unwrap();

        Self {
            cache: deserialized,
        }
    }
}

#[async_trait]
impl PreimageOracleClient for InMemoryOracle {
    async fn get(&self, key: PreimageKey) -> Result<Vec<u8>> {
        let lookup_key: [u8;32] = key.into();
        self.cache.get(&lookup_key).cloned().ok_or_else(|| anyhow!("Key not found in cache"))
    }

    async fn get_exact(&self, key: PreimageKey, buf: &mut [u8]) -> Result<()> {
        let lookup_key: [u8;32] = key.into();
        let value = self.cache.get(&lookup_key).ok_or_else(|| anyhow!("Key not found in cache"))?;
        buf.copy_from_slice(value.as_slice());
        Ok(())
    }
}


#[async_trait]
impl HintWriterClient for InMemoryOracle {
    async fn write(&self, _hint: &str) -> Result<()> {
        Ok(())
    }
}


#[derive(Default)]
struct Blob {
    // TODO: Advantage / disadvantage of using FixedBytes?
    commitment: FixedBytes<48>,
    // TODO: Import and use Blob type from kona-derive?
    data: FixedBytes<4096>,
    kzg_proof: FixedBytes<48>,
}

impl InMemoryOracle {
    pub fn verify(&self) -> Result<()> {
        let mut blobs: HashMap<FixedBytes<48>, Blob> = HashMap::new();

        for (key, value) in self.cache.iter() {
            let key: PreimageKey = <[u8; 32] as TryInto<PreimageKey>>::try_into(*key).unwrap();
            match key.key_type() {

                // These are public values so verification happens in Solidity.
                PreimageKeyType::Local => {
                    // no-op
                },
                // Validate that the hash of the value (with keccak key added) equals the key.
                PreimageKeyType::Keccak256 => {
                    let derived_key = PreimageKey::new(keccak256(value).into(), PreimageKeyType::Keccak256);
                    assert_eq!(key, derived_key, "zkvm keccak constraint failed!");
                },
                // Unimplemented.
                PreimageKeyType::GlobalGeneric => {
                    unimplemented!();
                },
                // Validate that the hash of the value (with sha256 key added) equals the key.
                PreimageKeyType::Sha256 => {
                    let derived_key: [u8; 32] = Sha256::digest(value).into();
                    // TODO: Confirm we don't need `derived_key[0] = 0x01; // VERSIONED_HASH_VERSION_KZG` because it's overwritten by PreimageKey
                    let derived_key = PreimageKey::new(derived_key, PreimageKeyType::Sha256);
                    assert_eq!(key, derived_key, "zkvm sha256 constraint failed!");
                },
                // Aggregate blobs and proofs in memory and verify after loop.
                PreimageKeyType::Blob => {
                    let blob_data_key: [u8;32] = PreimageKey::new(
                        key.try_into().unwrap(),
                        PreimageKeyType::Keccak256
                    ).into();

                    if let Some(blob_data) = self.cache.get(&blob_data_key) {
                        let commitment: FixedBytes<48> = blob_data[..48].try_into().unwrap();
                        let element: [u8; 8] = blob_data[72..].try_into().unwrap();
                        let element: u64 = u64::from_be_bytes(element);

                        // TODO: This requires upstream kona changes to save these values in L1Blob hint.
                        if element == 4096 {
                            // kzg_proof[0..32]
                            blobs
                                .entry(commitment)
                                .or_insert(Blob::default())
                                .kzg_proof[..32]
                                .copy_from_slice(value);
                            continue;
                        } else if element == 4097 {
                            // kzg_proof[32..48]
                            blobs
                                .entry(commitment)
                                .or_insert(Blob::default())
                                .kzg_proof[32..]
                                .copy_from_slice(&value[..16]);
                            continue;
                        }

                        // Add the 32 bytes of blob data into the correct spot in the blob.
                        blobs
                            .entry(commitment)
                            .or_insert(Blob::default())
                            .data
                            .get_mut((element as usize) << 5..(element as usize + 1) << 5)
                            .map(|slice| {
                                if slice.iter().all(|&byte| byte == 0) {
                                    slice.copy_from_slice(value);
                                    Ok(())
                                } else {
                                    return Err(anyhow!("trying to overwrite existing blob data"));
                                }
                            });
                    } else {
                        return Err(anyhow!("blob data not found"));
                    }
                },
                PreimageKeyType::Precompile => {
                    // Convert the Precompile type to a Keccak type. This is the key to get the hint data.
                    let hint_data_key: [u8;32] = PreimageKey::new(
                        key.try_into().unwrap(),
                        PreimageKeyType::Keccak256
                    ).into();

                    // Look up the hint data in the cache. It should always exist, because we only
                    // set Precompile KV pairs along with Keccak KV pairs for the hint data.
                    if let Some(hint_data) = self.cache.get(&hint_data_key) {
                        let precompile = Precompile::from_bytes(hint_data).unwrap();
                        let output = precompile.execute();
                        assert_eq!(value, &output, "zkvm precompile constraint failed!")
                    } else {
                        return Err(anyhow!("precompile hint data not found"));
                    }
                }
            }
        }

        // Verify reconstructed blobs.
        for (commitment, blob) in blobs.iter() {
            // kzg::verify_blob_kzg_proof(&blob.data, commitment, &blob.kzg_proof)
                // .map_err(|e| format!("blob verification failed for {:?}: {}", commitment, e))?;

            // TODO: Would this allow us to leave 000...000 segments in blobs that were not empty and prove that?
            // May need to track to ensure each blob element has been included.
        }

        Ok(())
    }
}
