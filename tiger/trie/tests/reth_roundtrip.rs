//! Round trip through a datadir base-reth wrote (set `TIGER_RETH_DATADIR` to a copy): tiger
//! unwinds the last blocks, then commits them again from changes read back through its own
//! historical views, copying the block data rows. Every root must match the headers, and the
//! state, trie, changeset and history tables must end up equal to what base-reth wrote. Set
//! `TIGER_ROUNDTRIP_OUT` to keep the rewritten datadir for opening with base-reth.

use std::{collections::BTreeSet, path::Path};

use alloy_primitives::{Address, B256, U256};
use base_tiger_store::{
    AccountBeforeTx, BlockBodyIndices, HeaderCodec, RocksTable, Segment, StorageBeforeTx, Table,
};
use base_tiger_trie::{BlockChanges, StateDb};

/// One block's data rows, copied as stored.
struct BlockRows {
    number: u64,
    changes: BlockChanges,
    state_root: B256,
    header: [Vec<u8>; 3],
    first_tx: u64,
    txs: Vec<Vec<u8>>,
    receipts: Vec<Vec<u8>>,
    senders: Vec<Vec<u8>>,
    mdbx: Vec<(Table, Vec<u8>, Vec<u8>)>,
    tx_hashes: Vec<(Vec<u8>, Vec<u8>)>,
}

fn table_entries(db: &StateDb, table: Table) -> Vec<(Vec<u8>, Vec<u8>)> {
    let d = &db.datadir.db;
    let txn = d.read().unwrap();
    let mut cursor = txn.cursor(d.dbi(table)).unwrap();
    let mut out = Vec::new();
    let mut entry = cursor.first().unwrap().map(|(k, v)| (k.to_vec(), v.to_vec()));
    while let Some(e) = entry {
        out.push(e);
        entry = cursor.next().unwrap().map(|(k, v)| (k.to_vec(), v.to_vec()));
    }
    out
}

fn read_block(db: &StateDb, number: u64) -> BlockRows {
    let sf = &db.datadir.static_files;
    let view = db.view(number, Vec::new()).unwrap();
    let mut changes = BlockChanges::default();
    let mut addresses = BTreeSet::new();
    for row in sf.changeset(Segment::AccountChangeSets, number).unwrap().unwrap() {
        addresses.insert(AccountBeforeTx::from_bytes(&row).unwrap().address);
    }
    let mut slots: BTreeSet<(Address, B256)> = BTreeSet::new();
    for row in sf.changeset(Segment::StorageChangeSets, number).unwrap().unwrap() {
        let row = StorageBeforeTx::from_bytes(&row).unwrap();
        slots.insert((row.address, row.slot));
    }
    for address in &addresses {
        let info = view.account(*address).unwrap();
        if let Some(info) = info
            && let Some(code) = view.code(info.code_hash).unwrap()
            && !code.is_empty()
        {
            changes.codes.push((info.code_hash, code));
        }
        changes.accounts.push((*address, info));
    }
    for (address, slot) in &slots {
        let slot = U256::from_be_bytes(slot.0);
        changes.storage.push((*address, slot, view.storage(*address, slot).unwrap()));
    }
    drop(view);
    let header = sf.header(number).unwrap().unwrap();
    let state_root = HeaderCodec::decode(&header[0]).unwrap().state_root;
    let d = &db.datadir.db;
    let txn = d.read().unwrap();
    let mut mdbx = Vec::new();
    let body = txn.get(d.dbi(Table::BlockBodyIndices), &number.to_be_bytes()).unwrap().unwrap().to_vec();
    mdbx.push((Table::BlockBodyIndices, number.to_be_bytes().to_vec(), body.clone()));
    mdbx.push((Table::HeaderNumbers, header[2].clone(), txn.get(d.dbi(Table::HeaderNumbers), &header[2]).unwrap().unwrap().to_vec()));
    drop(txn);
    for (key, value) in table_entries(db, Table::TransactionBlocks) {
        if value == number.to_le_bytes() {
            mdbx.push((Table::TransactionBlocks, key, value));
        }
    }
    let body = BlockBodyIndices::from_bytes(&body).unwrap();
    let rows = |segment| (body.first_tx..body.next_tx()).map(|tx| sf.tx_row(segment, tx).unwrap().unwrap()).collect();
    let tx_hashes = db
        .datadir
        .rocks
        .scan_prefix(RocksTable::TransactionHashNumbers, &[], usize::MAX)
        .unwrap()
        .into_iter()
        .filter(|(_, v)| (body.first_tx..body.next_tx()).contains(&u64::from_le_bytes(v[..8].try_into().unwrap())))
        .collect();
    BlockRows {
        number,
        changes,
        state_root,
        header,
        first_tx: body.first_tx,
        txs: rows(Segment::Transactions),
        receipts: rows(Segment::Receipts),
        senders: rows(Segment::TransactionSenders),
        mdbx,
        tx_hashes,
    }
}

/// Everything the round trip must reproduce.
fn snapshot(db: &StateDb, tip: u64) -> Vec<String> {
    let mut out = Vec::new();
    for table in [Table::HashedAccounts, Table::HashedStorages, Table::AccountsTrie, Table::StoragesTrie, Table::Bytecodes] {
        out.push(format!("{table:?} {:?}", table_entries(db, table)));
    }
    for table in [RocksTable::AccountsHistory, RocksTable::StoragesHistory, RocksTable::TransactionHashNumbers] {
        out.push(format!("{table:?} {:?}", db.datadir.rocks.scan_prefix(table, &[], usize::MAX).unwrap()));
    }
    let sf = &db.datadir.static_files;
    for block in 0..=tip {
        for segment in [Segment::AccountChangeSets, Segment::StorageChangeSets] {
            out.push(format!("{segment:?} {block} {:?}", sf.changeset(segment, block).unwrap()));
        }
        out.push(format!("header {block} {:?}", sf.header(block).unwrap()));
    }
    out
}

#[test]
fn unwind_and_recommit_reproduce_reths_datadir() {
    let Some(source) = std::env::var_os("TIGER_RETH_DATADIR") else { return };
    let source = Path::new(&source);
    let work = std::env::var_os("TIGER_ROUNDTRIP_OUT")
        .map(Into::into)
        .unwrap_or_else(|| std::env::temp_dir().join(format!("tiger-roundtrip-{}", std::process::id())));
    let _ = std::fs::remove_dir_all(&work);
    assert!(std::process::Command::new("cp").arg("-a").arg(source).arg(&work).status().unwrap().success());

    let db = StateDb::open(&work).unwrap();
    let tip = db.tip().unwrap().unwrap();
    let base = tip - 5;
    let before = snapshot(&db, tip);
    let blocks: Vec<BlockRows> = (base + 1..=tip).map(|n| read_block(&db, n)).collect();
    let base_root = HeaderCodec::decode(&db.datadir.static_files.header(base).unwrap().unwrap()[0]).unwrap().state_root;

    // Unwind, removing the node's block rows in the same transaction.
    let mut w = db.write().unwrap();
    let root = db.unwind_in(&mut w, base).unwrap();
    assert_eq!(root, base_root, "unwound root");
    for block in &blocks {
        for (table, key, _) in &block.mdbx {
            let dbi = w.db().dbi(*table);
            w.txn().delete(dbi, key, None).unwrap();
        }
        for (key, _) in &block.tx_hashes {
            w.rocks.delete(RocksTable::TransactionHashNumbers, key.clone());
        }
    }
    w.commit().unwrap();
    drop(db);
    let db = StateDb::open(&work).unwrap();
    assert_eq!(db.tip().unwrap(), Some(base));
    for segment in Segment::ALL {
        assert_eq!(db.datadir.static_files.last_block(segment), Some(base), "{segment:?}");
    }
    assert_eq!(db.view(base, Vec::new()).unwrap().state_root(None).unwrap(), base_root);

    // Commit the blocks again.
    for block in &blocks {
        let mut w = db.write().unwrap();
        let root = db.commit_block(&mut w, block.number, &block.changes).unwrap();
        assert_eq!(root, block.state_root, "block {}", block.number);
        let sf = &db.datadir.static_files;
        let [h, td, hash] = &block.header;
        sf.append_header(block.number, h, td, hash.as_slice().try_into().unwrap()).unwrap();
        sf.append_txs(Segment::Transactions, block.number, block.first_tx, &block.txs).unwrap();
        sf.append_txs(Segment::Receipts, block.number, block.first_tx, &block.receipts).unwrap();
        sf.append_txs(Segment::TransactionSenders, block.number, block.first_tx, &block.senders).unwrap();
        for (table, key, value) in &block.mdbx {
            let dbi = w.db().dbi(*table);
            w.txn().put(dbi, key, value).unwrap();
        }
        for (key, value) in &block.tx_hashes {
            w.rocks.put(RocksTable::TransactionHashNumbers, key.clone(), value.clone());
        }
        w.commit().unwrap();
    }
    drop(db);
    let db = StateDb::open(&work).unwrap();
    let after = snapshot(&db, tip);
    assert_eq!(before.len(), after.len());
    for (b, a) in before.iter().zip(&after) {
        assert_eq!(b, a);
    }
    drop(db);
    if std::env::var_os("TIGER_ROUNDTRIP_OUT").is_none() {
        let _ = std::fs::remove_dir_all(&work);
    }
}
