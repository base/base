use rocksdb::{
    ColumnFamilyDescriptor, DBCompressionType, Options, SliceTransform, DB,
};
use std::path::Path;

pub const CF_ACCOUNT_TRIE_HISTORY: &str = "account_trie_history";
pub const CF_STORAGE_TRIE_HISTORY: &str = "storage_trie_history";
pub const CF_HASHED_ACCOUNT_HISTORY: &str = "hashed_account_history";
pub const CF_HASHED_STORAGE_HISTORY: &str = "hashed_storage_history";
pub const CF_PROOF_WINDOW: &str = "proof_window";
pub const CF_BLOCK_CHANGE_SET: &str = "block_change_set";

pub const ALL_CF_NAMES: &[&str] = &[
    CF_ACCOUNT_TRIE_HISTORY,
    CF_STORAGE_TRIE_HISTORY,
    CF_HASHED_ACCOUNT_HISTORY,
    CF_HASHED_STORAGE_HISTORY,
    CF_PROOF_WINDOW,
    CF_BLOCK_CHANGE_SET,
];

const BLOCK_NUMBER_LEN: usize = 8;

fn cf_options_fixed_prefix(prefix_len: usize) -> Options {
    let mut opts = Options::default();
    opts.set_prefix_extractor(SliceTransform::create_fixed_prefix(prefix_len));
    opts.set_compression_type(DBCompressionType::None);
    opts
}

fn cf_options_no_prefix() -> Options {
    let mut opts = Options::default();
    opts.set_compression_type(DBCompressionType::None);
    opts
}

pub fn column_family_descriptors() -> Vec<ColumnFamilyDescriptor> {
    vec![
        // Variable-length prefix — no prefix extractor (use total_order_seek)
        ColumnFamilyDescriptor::new(CF_ACCOUNT_TRIE_HISTORY, cf_options_no_prefix()),
        // Variable-length prefix (B256 + StoredNibbles) — no prefix extractor
        ColumnFamilyDescriptor::new(CF_STORAGE_TRIE_HISTORY, cf_options_no_prefix()),
        // Fixed 32-byte prefix (B256 hashed address)
        ColumnFamilyDescriptor::new(CF_HASHED_ACCOUNT_HISTORY, cf_options_fixed_prefix(32)),
        // Fixed 64-byte prefix (B256 address + B256 slot)
        ColumnFamilyDescriptor::new(CF_HASHED_STORAGE_HISTORY, cf_options_fixed_prefix(64)),
        // Single-byte keys, no versioning
        ColumnFamilyDescriptor::new(CF_PROOF_WINDOW, cf_options_no_prefix()),
        // 8-byte keys, no versioning
        ColumnFamilyDescriptor::new(CF_BLOCK_CHANGE_SET, cf_options_no_prefix()),
    ]
}

pub fn open_rocksdb(path: &Path) -> Result<DB, rocksdb::Error> {
    let mut db_opts = Options::default();
    db_opts.create_if_missing(true);
    db_opts.create_missing_column_families(true);
    DB::open_cf_descriptors(&db_opts, path, column_family_descriptors())
}

pub fn encode_composite_key(key_bytes: &[u8], block_number: u64) -> Vec<u8> {
    let mut composite = Vec::with_capacity(key_bytes.len() + BLOCK_NUMBER_LEN);
    composite.extend_from_slice(key_bytes);
    composite.extend_from_slice(&block_number.to_be_bytes());
    composite
}

pub fn decode_composite_key(raw: &[u8]) -> (&[u8], u64) {
    assert!(
        raw.len() >= BLOCK_NUMBER_LEN,
        "composite key too short: {} bytes",
        raw.len()
    );
    let (key_bytes, block_bytes) = raw.split_at(raw.len() - BLOCK_NUMBER_LEN);
    let block_number = u64::from_be_bytes(block_bytes.try_into().expect("exactly 8 bytes"));
    (key_bytes, block_number)
}

pub fn encode_key_ceiling(key_bytes: &[u8]) -> Vec<u8> {
    encode_composite_key(key_bytes, u64::MAX)
}

pub fn encode_key_floor(key_bytes: &[u8]) -> Vec<u8> {
    encode_composite_key(key_bytes, 0)
}

pub fn key_prefix_matches(composite: &[u8], prefix: &[u8]) -> bool {
    composite.len() >= prefix.len() + BLOCK_NUMBER_LEN
        && &composite[..prefix.len()] == prefix
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_composite_key() {
        let key = b"hello";
        let block = 42u64;
        let encoded = encode_composite_key(key, block);
        let (decoded_key, decoded_block) = decode_composite_key(&encoded);
        assert_eq!(decoded_key, key);
        assert_eq!(decoded_block, block);
    }

    #[test]
    fn composite_key_ordering_same_prefix() {
        let key = b"abc";
        let a = encode_composite_key(key, 10);
        let b = encode_composite_key(key, 20);
        let c = encode_composite_key(key, u64::MAX);
        assert!(a < b);
        assert!(b < c);
    }

    #[test]
    fn composite_key_ordering_different_prefix() {
        let a = encode_composite_key(b"aaa", 100);
        let b = encode_composite_key(b"bbb", 1);
        assert!(a < b);
    }

    #[test]
    fn decode_empty_key_prefix() {
        let encoded = encode_composite_key(b"", 99);
        let (key, block) = decode_composite_key(&encoded);
        assert!(key.is_empty());
        assert_eq!(block, 99);
    }

    #[test]
    #[should_panic(expected = "composite key too short")]
    fn decode_too_short_panics() {
        decode_composite_key(&[1, 2, 3]);
    }

    #[test]
    fn key_prefix_matches_correct() {
        let key = b"hello";
        let composite = encode_composite_key(key, 42);
        assert!(key_prefix_matches(&composite, key));
        assert!(!key_prefix_matches(&composite, b"hell"));
        assert!(!key_prefix_matches(&composite, b"helloo"));
    }

    #[test]
    fn ceiling_and_floor() {
        let key = b"test";
        let floor = encode_key_floor(key);
        let ceiling = encode_key_ceiling(key);
        assert!(floor < ceiling);
        let (_, block_floor) = decode_composite_key(&floor);
        let (_, block_ceil) = decode_composite_key(&ceiling);
        assert_eq!(block_floor, 0);
        assert_eq!(block_ceil, u64::MAX);
    }

    #[test]
    fn open_rocksdb_creates_all_cfs() {
        let dir = tempfile::tempdir().unwrap();
        let db = open_rocksdb(dir.path()).unwrap();
        for cf_name in ALL_CF_NAMES {
            assert!(
                db.cf_handle(cf_name).is_some(),
                "missing column family: {cf_name}"
            );
        }
    }
}
