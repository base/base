//! Proof window metadata behavior tests.

use serial_test::serial;
use test_case::test_case;

use super::*;

/// Test basic storage and retrieval of earliest block number
#[test_case(InMemoryProofsStorage::new(); "InMemory")]
#[test_case(create_mdbx_proofs_storage(); "Mdbx")]
#[test_case(create_rocksdb_proofs_storage(); "Rocksdb")]
#[serial]
fn test_earliest_block_operations<S: BaseProofsStore + BaseProofsInitialStateStore>(
    storage: S,
) -> Result<(), BaseProofsStorageError> {
    // Initially should be None
    let earliest = storage.get_earliest_block_number()?;
    assert!(earliest.is_none());

    // Set earliest block
    let block_hash = B256::repeat_byte(0x42);
    storage.set_earliest_block_number(100, block_hash)?;

    // Should retrieve the same values
    let earliest = storage.get_earliest_block_number()?;
    assert_eq!(earliest, Some((100, block_hash)));

    Ok(())
}

#[test_case(InMemoryProofsStorage::new(); "InMemory")]
#[test_case(create_mdbx_proofs_storage(); "Mdbx")]
#[test_case(create_rocksdb_proofs_storage(); "Rocksdb")]
#[serial]
fn test_proof_window<S: BaseProofsStore + BaseProofsInitialStateStore>(
    storage: S,
) -> Result<(), BaseProofsStorageError> {
    assert_eq!(storage.get_earliest_block_number()?, None);
    let block_hash_42 = B256::repeat_byte(0x42);
    storage.set_earliest_block_number(42, block_hash_42)?;
    assert_eq!(storage.get_earliest_block_number()?, Some((42, block_hash_42)));

    let block_hash_100 = B256::repeat_byte(0x64);
    storage.set_earliest_block_number(100, block_hash_100)?;
    assert_eq!(storage.get_earliest_block_number()?, Some((100, block_hash_100)));
    assert_eq!(storage.get_latest_block_number()?, Some((100, block_hash_100)));
    Ok(())
}
