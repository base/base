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

/// A [`PreimageStore`] read by the proof program, where a missing non-local key panics.
///
/// Shared derivation code treats some oracle errors as protocol outcomes. For example, span batch
/// validation marks a batch undecided when its parent or overlapped L2 blocks cannot be read, and
/// the batch stream then drops it. The prover chooses which preimages the witness contains, so a
/// recoverable error would let it choose those outcomes. Panicking makes a witness with a missing
/// preimage unprovable instead.
///
/// Missing local keys still return [`PreimageOracleError::InvalidPreimageKey`]. Local keys carry
/// boot inputs rather than chain data, and `BootInfo::get_optional_local` relies on that error to
/// apply defaults for optional boot fields.
///
/// [`PreimageStore`] itself keeps returning an error for every missing key, because witness
/// collection relies on it to fall back to the host.
#[derive(Clone, Debug)]
pub struct WitnessOracle(PreimageStore);

impl WitnessOracle {
    /// Wraps witness preimages for reading by the proof program.
    ///
    /// Callers must validate the preimages first; use
    /// [`WitnessData::get_oracle_and_blob_provider`](super::WitnessData::get_oracle_and_blob_provider).
    pub const fn new(store: PreimageStore) -> Self {
        Self(store)
    }
}

#[async_trait]
impl HintWriterClient for WitnessOracle {
    async fn write(&self, _hint: &str) -> PreimageOracleResult<()> {
        Ok(())
    }
}

#[async_trait]
impl PreimageOracleClient for WitnessOracle {
    async fn get(&self, key: PreimageKey) -> PreimageOracleResult<Vec<u8>> {
        if let Some(value) = self.0.preimage_map.get(&key) {
            return Ok(value.clone());
        }
        if key.key_type() == PreimageKeyType::Local {
            return Err(PreimageOracleError::InvalidPreimageKey);
        }
        panic!("requested preimage key not present in witness: {key}");
    }

    async fn get_exact(&self, key: PreimageKey, buf: &mut [u8]) -> PreimageOracleResult<()> {
        buf.copy_from_slice(&self.get(key).await?);
        Ok(())
    }
}

impl FlushableCache for WitnessOracle {
    fn flush(&self) {}
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use alloy_consensus::{EMPTY_ROOT_HASH, Header};
    use alloy_eips::BlockNumHash;
    use alloy_primitives::{B256, FixedBytes};
    use base_common_genesis::{ChainGenesis, RollupConfig, UpgradeConfig};
    use base_proof::{OracleL2ChainProvider, block_on};
    use base_protocol::{BatchValidity, BlockInfo, L2BlockInfo, SpanBatch, SpanBatchElement};

    use super::*;
    use crate::witness::{BlobData, DefaultWitnessData, WitnessData};

    struct OverlappingSpanBatch {
        witness: PreimageStore,
        config: RollupConfig,
        l1_origin: BlockInfo,
        safe_head: L2BlockInfo,
        batch: SpanBatch,
    }

    impl OverlappingSpanBatch {
        /// Builds safe head #1 on genesis #0 and a span batch whose first element overlaps the
        /// safe head, so prefix validation must read the genesis header from the witness.
        fn new(withhold_parent_header: bool) -> Self {
            let l1_origin = BlockInfo {
                hash: B256::repeat_byte(0x11),
                number: 1,
                timestamp: 100,
                ..Default::default()
            };
            let parent = Header { number: 0, timestamp: 100, ..Default::default() };
            let parent_hash = parent.hash_slow();
            let safe_head = Header { number: 1, timestamp: 102, parent_hash, ..Default::default() };
            let safe_head_hash = safe_head.hash_slow();

            let mut witness = PreimageStore::default();
            witness
                .save_preimage(PreimageKey::new_keccak256(*EMPTY_ROOT_HASH), vec![0x80])
                .unwrap();
            witness
                .save_preimage(
                    PreimageKey::new_keccak256(*safe_head_hash),
                    alloy_rlp::encode(&safe_head),
                )
                .unwrap();
            if !withhold_parent_header {
                witness
                    .save_preimage(
                        PreimageKey::new_keccak256(*parent_hash),
                        alloy_rlp::encode(&parent),
                    )
                    .unwrap();
            }

            let config = RollupConfig {
                block_time: 2,
                seq_window_size: 100,
                genesis: ChainGenesis {
                    l1: BlockNumHash { number: 1, hash: l1_origin.hash },
                    l2: BlockNumHash { number: 0, hash: parent_hash },
                    l2_time: 100,
                    ..Default::default()
                },
                upgrades: UpgradeConfig {
                    delta_time: Some(0),
                    holocene_time: Some(0),
                    ..Default::default()
                },
                ..Default::default()
            };

            Self {
                witness,
                config,
                l1_origin,
                safe_head: L2BlockInfo {
                    block_info: BlockInfo {
                        hash: safe_head_hash,
                        number: 1,
                        parent_hash,
                        timestamp: 102,
                    },
                    l1_origin: l1_origin.id(),
                    seq_num: 1,
                },
                batch: SpanBatch {
                    parent_check: FixedBytes::from_slice(&parent_hash[..20]),
                    l1_origin_check: FixedBytes::from_slice(&l1_origin.hash[..20]),
                    batches: vec![
                        SpanBatchElement { epoch_num: 1, timestamp: 102, transactions: vec![] },
                        SpanBatchElement { epoch_num: 1, timestamp: 104, transactions: vec![] },
                    ],
                    ..Default::default()
                },
            }
        }

        fn check_prefix_through_guest_oracle(self) -> BatchValidity {
            block_on(async {
                let (oracle, _) = DefaultWitnessData::from_parts(self.witness, BlobData::default())
                    .get_oracle_and_blob_provider()
                    .await
                    .unwrap();
                let config = Arc::new(self.config);
                let mut provider = OracleL2ChainProvider::new(
                    self.safe_head.block_info.hash,
                    Arc::clone(&config),
                    oracle,
                );
                self.batch
                    .check_batch_prefix(
                        &config,
                        &[self.l1_origin],
                        self.safe_head,
                        &self.l1_origin,
                        &mut provider,
                    )
                    .await
                    .0
            })
        }
    }

    #[test]
    fn complete_witness_accepts_overlapping_span_batch() {
        assert_eq!(
            OverlappingSpanBatch::new(false).check_prefix_through_guest_oracle(),
            BatchValidity::Accept
        );
    }

    #[test]
    #[should_panic(expected = "requested preimage key not present in witness")]
    fn missing_witness_preimage_aborts_span_batch_validation() {
        OverlappingSpanBatch::new(true).check_prefix_through_guest_oracle();
    }

    #[test]
    fn missing_local_key_remains_recoverable_for_optional_boot_fields() {
        let oracle = WitnessOracle::new(PreimageStore::default());

        assert!(matches!(
            block_on(oracle.get(PreimageKey::new_local(1))),
            Err(PreimageOracleError::InvalidPreimageKey)
        ));
    }

    #[test]
    fn preimage_store_returns_error_for_host_fallback() {
        let store = PreimageStore::default();

        assert!(matches!(
            block_on(store.get(PreimageKey::new_keccak256([0x22; 32]))),
            Err(PreimageOracleError::InvalidPreimageKey)
        ));
    }
}
