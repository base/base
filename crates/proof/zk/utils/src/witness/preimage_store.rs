use std::collections::{HashMap, hash_map::Entry};

use alloy_primitives::keccak256;
use async_trait::async_trait;
use base_proof_preimage::{
    FlushableCache, HintWriterClient, PreimageKey, PreimageKeyType, PreimageOracleClient,
    errors::{PreimageOracleError, PreimageOracleResult},
};
use serde::{Deserialize, Serialize};
use sha2::Digest;

/// In-memory store of preimage key-value pairs for the zkVM oracle.
#[derive(
    Clone, Debug, Default, Serialize, Deserialize, rkyv::Serialize, rkyv::Archive, rkyv::Deserialize,
)]
pub struct PreimageStore {
    /// Map of preimage keys to their values.
    #[serde(with = "preimage_map_serde")]
    pub preimage_map: HashMap<PreimageKey, Vec<u8>>,
}

/// Serialize/deserialize `HashMap<PreimageKey, Vec<u8>>` as a sequence of `(PreimageKey, Vec<u8>)`
/// pairs. This avoids the serde requirement that map keys serialize as strings.
mod preimage_map_serde {
    use serde::{
        de::Deserializer,
        ser::{SerializeSeq, Serializer},
    };

    use super::{Deserialize, HashMap, PreimageKey};

    pub(super) fn serialize<S: Serializer>(
        map: &HashMap<PreimageKey, Vec<u8>>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        let mut seq = serializer.serialize_seq(Some(map.len()))?;
        for (k, v) in map {
            seq.serialize_element(&(k, v))?;
        }
        seq.end()
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<HashMap<PreimageKey, Vec<u8>>, D::Error> {
        let pairs: Vec<(PreimageKey, Vec<u8>)> = Deserialize::deserialize(deserializer)?;
        Ok(pairs.into_iter().collect())
    }
}

impl PreimageStore {
    /// Validate all stored preimages against their key hashes.
    pub fn check_preimages(&self) -> PreimageOracleResult<()> {
        for (key, value) in &self.preimage_map {
            check_preimage(key, value)?;
        }
        Ok(())
    }

    /// Insert a preimage, rejecting overwrites with different values.
    pub fn save_preimage(&mut self, key: PreimageKey, value: Vec<u8>) -> PreimageOracleResult<()> {
        check_preimage(&key, &value)?;

        match self.preimage_map.entry(key) {
            Entry::Vacant(e) => {
                e.insert(value);
            }
            Entry::Occupied(e) => {
                if e.get() != &value {
                    return Err(PreimageOracleError::Other("cannot overwrite key".to_string()));
                }
            }
        };

        Ok(())
    }
}

/// Check that the preimage matches the expected hash.
pub fn check_preimage(key: &PreimageKey, value: &[u8]) -> PreimageOracleResult<()> {
    if let Some(expected_hash) = match key.key_type() {
        PreimageKeyType::Keccak256 => Some(keccak256(value).0),
        PreimageKeyType::Sha256 => Some(sha2::Sha256::digest(value).into()),
        PreimageKeyType::Local | PreimageKeyType::GlobalGeneric => None,
        PreimageKeyType::Precompile => unimplemented!("Precompile not supported in zkVM"),
        PreimageKeyType::Blob => unreachable!("Blob keys validated in blob witness"),
    } && key != &PreimageKey::new(expected_hash, key.key_type())
    {
        return Err(PreimageOracleError::InvalidPreimageKey);
    }
    Ok(())
}

#[async_trait]
impl HintWriterClient for PreimageStore {
    async fn write(&self, _hint: &str) -> PreimageOracleResult<()> {
        Ok(())
    }
}

#[async_trait]
impl PreimageOracleClient for PreimageStore {
    async fn get(&self, key: PreimageKey) -> PreimageOracleResult<Vec<u8>> {
        let Some(value) = self.preimage_map.get(&key) else {
            return Err(PreimageOracleError::InvalidPreimageKey);
        };
        Ok(value.clone())
    }

    async fn get_exact(&self, key: PreimageKey, buf: &mut [u8]) -> PreimageOracleResult<()> {
        buf.copy_from_slice(&self.get(key).await?);
        Ok(())
    }
}

impl FlushableCache for PreimageStore {
    fn flush(&self) {}
}
