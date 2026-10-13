use std::{
    fs::{self, File},
    io::{self, Write},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use alloy_primitives::{B256, FixedBytes};

use crate::BaseEvmConfig;

/// The authorization recorded for one block's native BLS transfers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlsStoredWitness {
    /// The aggregate of every included transfer's individual signature, published with the
    /// block so it can be verified again without the pooled witnesses.
    Aggregate(FixedBytes<48>),
    /// The block was derived from authenticated input, which carries no per-block signature.
    Derived,
}

impl BlsStoredWitness {
    const DERIVED_TAG: u8 = 0;
    const AGGREGATE_TAG: u8 = 1;

    /// Encodes the tagged durable witness record.
    pub fn encode(&self) -> Vec<u8> {
        match self {
            Self::Derived => vec![Self::DERIVED_TAG],
            Self::Aggregate(aggregate) => {
                let mut out = Vec::with_capacity(1 + aggregate.len());
                out.push(Self::AGGREGATE_TAG);
                out.extend_from_slice(aggregate.as_slice());
                out
            }
        }
    }

    /// Decodes an exact durable witness record, rejecting unknown tags and invalid lengths.
    pub fn decode(bytes: &[u8]) -> io::Result<Self> {
        match bytes {
            [Self::DERIVED_TAG] => Ok(Self::Derived),
            [Self::AGGREGATE_TAG, aggregate @ ..] if aggregate.len() == 48 => {
                Ok(Self::Aggregate(FixedBytes::from_slice(aggregate)))
            }
            _ => Err(io::Error::new(io::ErrorKind::InvalidData, "malformed BLS witness record")),
        }
    }
}

/// Durable per-block store of native BLS block authorizations, keyed by canonical block hash.
///
/// Each record lives in its own file named by the block hash and is replaced atomically
/// (write to a temporary file, sync, rename, sync the directory), so a crash leaves either
/// the previous record or the new one, never a torn record.
///
/// A block's aggregate is authoritative once recorded: [`Self::put`] keeps it when the same
/// block later arrives as derived, and rejects a different aggregate.
#[derive(Debug)]
pub struct BlsWitnessStore {
    dir: PathBuf,
    next_temp: AtomicU64,
    /// Serializes the read-merge-write of [`Self::put`].
    write_lock: Mutex<()>,
}

impl BlsWitnessStore {
    /// Opens the store rooted at `dir`, creating the directory when missing.
    pub fn open(dir: PathBuf) -> io::Result<Self> {
        fs::create_dir_all(&dir)?;
        Ok(Self { dir, next_temp: AtomicU64::new(0), write_lock: Mutex::new(()) })
    }

    /// Returns the witness recorded for `block_hash`, if any.
    pub fn get(&self, block_hash: B256) -> io::Result<Option<BlsStoredWitness>> {
        match fs::read(self.dir.join(block_hash.to_string())) {
            Ok(bytes) => BlsStoredWitness::decode(&bytes).map(Some),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Durably records `witness` for `block_hash`.
    ///
    /// An aggregate replaces a derived marker. A derived marker or the same aggregate leaves
    /// an existing aggregate in place, and a different aggregate is an
    /// [`io::ErrorKind::AlreadyExists`] error.
    pub fn put(&self, block_hash: B256, witness: BlsStoredWitness) -> io::Result<()> {
        let _guard = self.write_lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        match (self.get(block_hash)?, witness) {
            (Some(BlsStoredWitness::Aggregate(_)), BlsStoredWitness::Derived) => return Ok(()),
            (Some(existing), _) if existing == witness => return Ok(()),
            (Some(BlsStoredWitness::Aggregate(_)), BlsStoredWitness::Aggregate(_)) => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "a different BLS aggregate is already recorded for this block",
                ));
            }
            _ => {}
        }
        let temp = self.dir.join(format!(
            ".{block_hash}.{}.{}.tmp",
            std::process::id(),
            self.next_temp.fetch_add(1, Ordering::Relaxed)
        ));
        let result = (|| {
            let mut file = File::create(&temp)?;
            file.write_all(&witness.encode())?;
            file.sync_all()?;
            fs::rename(&temp, self.dir.join(block_hash.to_string()))?;
            File::open(&self.dir)?.sync_all()
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temp);
        }
        result
    }
}

/// Access to a node component's durable [`BlsWitnessStore`], so generic node wiring can share
/// the one store the EVM configuration was built with.
pub trait BlsWitnessStoreProvider {
    /// Returns the store, if this node keeps one.
    fn bls_witness_store(&self) -> Option<&Arc<BlsWitnessStore>>;
}

impl<ChainSpec, N: reth_primitives_traits::NodePrimitives, R, EvmFactory> BlsWitnessStoreProvider
    for BaseEvmConfig<ChainSpec, N, R, EvmFactory>
{
    fn bls_witness_store(&self) -> Option<&Arc<BlsWitnessStore>> {
        self.bls_witness_store.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> PathBuf {
        std::env::temp_dir().join(format!(
            "bls-witness-store-{}-{}",
            std::process::id(),
            B256::random()
        ))
    }

    #[test]
    fn records_survive_reopen_and_keep_the_first_aggregate() {
        let dir = temp_dir();
        let derived = B256::repeat_byte(1);
        let aggregated = B256::repeat_byte(2);
        let aggregate = FixedBytes::<48>::repeat_byte(0xab);
        {
            let store = BlsWitnessStore::open(dir.clone()).unwrap();
            assert_eq!(store.get(derived).unwrap(), None);
            store.put(derived, BlsStoredWitness::Derived).unwrap();
            store.put(derived, BlsStoredWitness::Derived).unwrap();
            // An aggregate upgrades a derived marker, and is kept against a later derived
            // marker or a repeat of itself.
            store.put(aggregated, BlsStoredWitness::Derived).unwrap();
            store.put(aggregated, BlsStoredWitness::Aggregate(aggregate)).unwrap();
            store.put(aggregated, BlsStoredWitness::Derived).unwrap();
            store.put(aggregated, BlsStoredWitness::Aggregate(aggregate)).unwrap();
            let conflict = FixedBytes::<48>::repeat_byte(0xcd);
            assert_eq!(
                store.put(aggregated, BlsStoredWitness::Aggregate(conflict)).unwrap_err().kind(),
                io::ErrorKind::AlreadyExists
            );
        }

        let store = BlsWitnessStore::open(dir.clone()).unwrap();
        assert_eq!(store.get(derived).unwrap(), Some(BlsStoredWitness::Derived));
        assert_eq!(store.get(aggregated).unwrap(), Some(BlsStoredWitness::Aggregate(aggregate)));
        assert_eq!(store.get(B256::repeat_byte(3)).unwrap(), None);
        // Only the two records remain; no temporary files are left behind.
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 2);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn corrupt_records_are_errors_not_missing() {
        let dir = temp_dir();
        let store = BlsWitnessStore::open(dir.clone()).unwrap();
        let hash = B256::repeat_byte(4);
        fs::write(dir.join(hash.to_string()), [1, 2, 3]).unwrap();
        assert_eq!(store.get(hash).unwrap_err().kind(), io::ErrorKind::InvalidData);
        fs::remove_dir_all(dir).unwrap();
    }
}
