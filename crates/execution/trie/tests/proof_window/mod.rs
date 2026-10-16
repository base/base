//! Proof window metadata behavior tests.

use rstest::rstest;
use serial_test::serial;

use super::*;

#[rstest]
#[case::in_memory(InMemoryProofsStorage::new())]
#[case::mdbx(create_mdbx_proofs_storage())]
#[case::rocksdb(create_rocksdb_proofs_storage())]
#[serial]
fn test_proof_window<S: BaseProofsStore + BaseProofsInitialStateStore>(
    #[case] storage: S,
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
