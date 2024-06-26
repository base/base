//! Contains the host <-> client communication utilities.

use super::Precompile;

use alloc::{boxed::Box, vec::Vec};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use kona_preimage::{HintWriterClient, PreimageKey, PreimageOracleClient};
use std::collections::HashMap;
use sha2::{Digest, Sha256};
use rkyv::{Archive, Serialize, Deserialize, Infallible};
use zkvm_common::BytesHasherBuilder;

#[derive(Debug, Clone, Archive, Serialize, Deserialize)]
pub struct InMemoryOracle {
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
        let lookup_key: [u8; 32] = key.into();
        self.cache.get(&lookup_key).cloned().ok_or_else(|| anyhow!("Key not found in cache"))
    }

    async fn get_exact(&self, key: PreimageKey, buf: &mut [u8]) -> Result<()> {
        let lookup_key: [u8; 32] = key.into();
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

// #[derive(Default)]
// struct Blob {
//     // TODO: Advantage / disadvantage of using FixedBytes?
//     commitment: FixedBytes<48>,
//     // TODO: Import and use Blob type from kona-derive?
//     data: FixedBytes<4096>,
//     kzg_proof: FixedBytes<48>,
// }

impl InMemoryOracle {
    pub fn verify(&self) -> Result<()> {
        // let mut blobs: HashMap<FixedBytes<48>, Blob> = HashMap::new();

        // for (key, value) in self.cache.iter() {
        //     match key.key_type() {
        //         PreimageKeyType::Local => {
        //             // no op - these are public values so verification happens in solidity
        //         },
        //         PreimageKeyType::Keccak256 => {
        //             let derived_key = PreimageKey::new(keccak256(value).into(), PreimageKeyType::Keccak256);
        //             assert_eq!(*key, derived_key, "zkvm keccak constraint failed!");
        //         },
        //         PreimageKeyType::GlobalGeneric => {
        //             unimplemented!();
        //         },
        //         PreimageKeyType::Sha256 => {
        //             let derived_key: [u8; 32] = Sha256::digest(value).into();
        //             // TODO: Confirm we don't need `derived_key[0] = 0x01; // VERSIONED_HASH_VERSION_KZG` because it's overwritten by PreimageKey
        //             let derived_key = PreimageKey::new(derived_key, PreimageKeyType::Sha256);
        //             assert_eq!(*key, derived_key, "zkvm sha256 constraint failed!");
        //         },
        //         PreimageKeyType::Blob => {
        //             let blob_data_key = PreimageKey::new(
        //                 <PreimageKey as Into<[u8;32]>>::into(*key),
        //                 PreimageKeyType::Keccak256
        //             );

        //             if let Some(blob_data) = self.cache.get(&blob_data_key) {
        //                 let commitment: FixedBytes<48> = blob_data[..48].try_into().unwrap();
        //                 let element: [u8; 8] = blob_data[72..].try_into().unwrap();
        //                 let element: u64 = u64::from_be_bytes(element);

        //                 if element == 4096 {
        //                     // This is the proof for the blob
        //                     blobs
        //                         .entry(commitment)
        //                         .or_insert(Blob::default())
        //                         .kzg_proof[..32]
        //                         .copy_from_slice(value);
        //                     continue;
        //                 } else if element == 4097 {
        //                     // This is the commitment for the blob
        //                     blobs
        //                         .entry(commitment)
        //                         .or_insert(Blob::default())
        //                         .kzg_proof[32..]
        //                         .copy_from_slice(&value[..16]);
        //                     continue;
        //                 }

        //                 // value is 32 byte segment from blob, so insert it in the right place
        //                 blobs
        //                     .entry(commitment)
        //                     .or_insert(Blob::default())
        //                     .data
        //                     .get_mut((element as usize) << 5..(element as usize + 1) << 5)
        //                     .map(|slice| {
        //                         if slice.iter().all(|&byte| byte == 0) {
        //                             slice.copy_from_slice(value);
        //                             Ok(())
        //                         } else {
        //                             return Err(anyhow!("trying to overwrite existing blob data"));
        //                         }
        //                     });
        //             } else {
        //                 return Err(anyhow!("blob data not found"));
        //             }
        //             // Aggregate blobs and proofs in memory and verify after loop.
        //             // Check that range is empty then add it (should be guaranteed because can't add twice, can optimize out later)
        //         },
        //         PreimageKeyType::Precompile => {
        //             // Convert the Precompile type to a Keccak type. This is the key to get the hint data.
        //             let hint_data_key = PreimageKey::new(
        //                 <PreimageKey as Into<[u8;32]>>::into(*key),
        //                 PreimageKeyType::Keccak256
        //             );

        //             // Look up the hint data in the cache. It should always exist, because we only
        //             // set Precompile KV pairs along with Keccak KV pairs for the hint data.
        //             if let Some(hint_data) = self.cache.get(&hint_data_key) {
        //                 let precompile = Precompile::from_bytes(hint_data).unwrap();
        //                 let output = precompile.execute();
        //                 assert_eq!(value, &output, "zkvm precompile constraint failed!")
        //             } else {
        //                 return Err(anyhow!("precompile hint data not found"));
        //             }
        //         }
        //     }
        // }

        // // Blob verification of complete blobs goes here.
        // for (commitment, blob) in blobs.iter() {
        //     // call out to kzg-rs to verify that the full blob is valid
        //     // kzg::verify_blob_kzg_proof(&blob.data, commitment, &blob.kzg_proof)
        //         // .map_err(|e| format!("blob verification failed for {:?}: {}", commitment, e))?;

        //     // TODO: I think allows us to leave empty segments in blobs that were not empty and prove that?
        // }

        Ok(())
    }
}
