//! Contains the host <-> client communication utilities.

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
