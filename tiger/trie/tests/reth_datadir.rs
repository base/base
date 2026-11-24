//! Checks against a datadir base-reth wrote. Set `TIGER_RETH_DATADIR` to a copy of one (the
//! tests open it for writing); they skip otherwise.

use alloy_primitives::B256;
use alloy_trie::{HashBuilder, Nibbles};
use base_tiger_store::{Db, Table, TrieCodec};
use base_tiger_trie::{HashedCursor, HashedState, HashedAccountCursor, HashedStorageCursor, TrieView, account_leaf, storage_leaf};

fn datadir() -> Option<std::path::PathBuf> {
    std::env::var_os("TIGER_RETH_DATADIR").map(Into::into)
}

#[test]
fn incremental_root_and_rebuilt_nodes_match_reth() {
    let Some(dir) = datadir() else { return };
    let db = Db::open(&dir.join("db")).unwrap();
    let txn = db.read().unwrap();
    // From scratch: every leaf, no stored nodes.
    let mut accounts = Vec::new();
    let mut cursor = HashedAccountCursor::new(&txn, &db).unwrap();
    let mut entry = cursor.seek(B256::ZERO).unwrap();
    while let Some((key, account)) = entry {
        accounts.push((key, account));
        entry = cursor.next().unwrap();
    }
    let mut builder = HashBuilder::default();
    for (key, account) in &accounts {
        let mut storage = HashBuilder::default();
        let mut cursor = HashedStorageCursor::new(&txn, &db, *key).unwrap();
        let mut entry = cursor.seek(B256::ZERO).unwrap();
        while let Some((slot, value)) = entry {
            storage.add_leaf(Nibbles::unpack(slot), &storage_leaf(value));
            entry = cursor.next().unwrap();
        }
        builder.add_leaf(Nibbles::unpack(*key), &account_leaf(account, storage.root()));
    }
    let scratch = builder.root();
    let empty = HashedState::default();
    let (root, updates) = TrieView::new(&txn, &db, &empty).state_root(true).unwrap();
    assert_eq!(root, scratch);
    assert!(updates.storage_tries.is_empty());
    let noop_updates = updates.account_nodes.clone();
    println!("accounts {} root {root}", accounts.len());

    // Rebuild every node from the leaves alone and compare with what reth stored.
    let mut all = HashedState::default();
    for (key, account) in &accounts {
        all.set_account(*key, *account);
        all.wipe_storage(*key);
        let mut cursor = HashedStorageCursor::new(&txn, &db, *key).unwrap();
        let mut entry = cursor.seek(B256::ZERO).unwrap();
        while let Some((slot, value)) = entry {
            all.set_slot(*key, slot, value);
            entry = cursor.next().unwrap();
        }
    }
    let (rebuilt_root, rebuilt) = TrieView::new(&txn, &db, &all).state_root(true).unwrap();
    assert_eq!(rebuilt_root, root);
    let mut stored = std::collections::BTreeMap::new();
    let mut cursor = txn.cursor(db.dbi(Table::AccountsTrie)).unwrap();
    let mut entry = cursor.first().unwrap().map(|(k, v)| (k.to_vec(), v.to_vec()));
    while let Some((k, v)) = entry {
        stored.insert(TrieCodec::unpack_key(&k).unwrap(), TrieCodec::decode_node(&v).unwrap());
        entry = cursor.next().unwrap().map(|(k, v)| (k.to_vec(), v.to_vec()));
    }
    let rebuilt_nodes: std::collections::BTreeMap<_, _> =
        rebuilt.account_nodes.iter().filter_map(|(p, n)| n.clone().map(|n| (*p, n))).collect();
    assert_eq!(rebuilt_nodes, stored, "account trie nodes");
    // With no changes, updates only rewrite stored nodes unchanged, as reth's do.
    for (path, node) in &noop_updates {
        assert_eq!(node.as_ref(), stored.get(path), "no-op update at {path:?}");
    }
    let mut storage_nodes = 0;
    for (key, updates) in &rebuilt.storage_tries {
        let mut stored = std::collections::BTreeMap::new();
        let mut cursor = txn.cursor(db.dbi(Table::StoragesTrie)).unwrap();
        let mut entry = cursor.seek_exact(key.as_slice()).unwrap().map(|(_, v)| v.to_vec());
        while let Some(v) = entry {
            let (path, node) = TrieCodec::decode_storage_entry(&v).unwrap();
            stored.insert(path, node);
            entry = cursor.next_dup().unwrap().map(|(_, v)| v.to_vec());
        }
        let rebuilt_nodes: std::collections::BTreeMap<_, _> =
            updates.nodes.iter().filter_map(|(p, n)| n.clone().map(|n| (*p, n))).collect();
        storage_nodes += rebuilt_nodes.len();
        assert_eq!(rebuilt_nodes, stored, "storage trie nodes of {key}");
    }
    assert_eq!(storage_nodes as u64, txn.entries(db.dbi(Table::StoragesTrie)).unwrap());
}
