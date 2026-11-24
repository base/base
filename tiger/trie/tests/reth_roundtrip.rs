//! Round trip through a datadir base-reth wrote (set `TIGER_RETH_DATADIR` to a copy): tiger
//! unwinds the last blocks, then commits them again from changes read back through its own
//! historical views, copying the block data rows. Every root must match the headers, and the
//! state, trie, changeset and history tables must end up equal to what base-reth wrote. Set
//! `TIGER_ROUNDTRIP_OUT` to keep the rewritten datadir for opening with base-reth.

use std::{collections::BTreeSet, path::Path};

use alloy_consensus::Header;
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Address, B256, U256};
use base_common_consensus::{BaseReceipt, BaseTxEnvelope};
use base_tiger_store::{AccountBeforeTx, RocksTable, Segment, StorageBeforeTx, Table};
use base_tiger_trie::{BlockChanges, StateDb};

/// One block as read back through tiger's block and state reads.
struct BlockRows {
    number: u64,
    changes: BlockChanges,
    header: Header,
    hash: B256,
    transactions: Vec<BaseTxEnvelope>,
    senders: Vec<Address>,
    receipts: Vec<BaseReceipt>,
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
    let dd = &db.datadir;
    let (header, hash) = dd.header(number).unwrap().unwrap();
    assert_eq!(header.hash_slow(), hash);
    assert_eq!(dd.block_number(hash).unwrap(), Some(number));
    let transactions = dd.transactions(number).unwrap().unwrap();
    for (index, tx) in transactions.iter().enumerate() {
        assert_eq!(dd.transaction_location(tx.trie_hash()).unwrap(), Some((number, index)));
    }
    BlockRows {
        number,
        changes,
        header,
        hash,
        transactions,
        senders: dd.senders(number).unwrap().unwrap(),
        receipts: dd.receipts(number).unwrap().unwrap(),
    }
}

/// Everything the round trip must reproduce.
fn snapshot(db: &StateDb, tip: u64) -> Vec<String> {
    let mut out = Vec::new();
    for table in [
        Table::HashedAccounts,
        Table::HashedStorages,
        Table::AccountsTrie,
        Table::StoragesTrie,
        Table::Bytecodes,
        Table::HeaderNumbers,
        Table::BlockBodyIndices,
        Table::TransactionBlocks,
        Table::StageCheckpoints,
    ] {
        out.push(format!("{table:?} {:?}", table_entries(db, table)));
    }
    for table in [RocksTable::AccountsHistory, RocksTable::StoragesHistory, RocksTable::TransactionHashNumbers] {
        out.push(format!("{table:?} {:?}", db.datadir.rocks.scan_prefix(table, &[], usize::MAX).unwrap()));
    }
    let sf = &db.datadir.static_files;
    let first = sf.first_block(Segment::AccountChangeSets).unwrap();
    for block in first..=tip {
        for segment in [Segment::AccountChangeSets, Segment::StorageChangeSets] {
            out.push(format!("{segment:?} {block} {:?}", sf.changeset(segment, block).unwrap()));
        }
        out.push(format!("header {block} {:?}", sf.header(block).unwrap()));
    }
    for segment in [Segment::Transactions, Segment::Receipts, Segment::TransactionSenders] {
        let rows: Vec<_> = (0..sf.next_tx(segment)).map(|tx| sf.tx_row(segment, tx).unwrap()).collect();
        out.push(format!("{segment:?} {rows:?}"));
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
    let base_root = db.datadir.header(base).unwrap().unwrap().0.state_root;

    // Unwind: the state and the block rows.
    let root = db.unwind_to(base).unwrap();
    assert_eq!(root, base_root, "unwound root");
    for block in &blocks {
        assert_eq!(db.datadir.block_number(block.hash).unwrap(), None);
        for tx in &block.transactions {
            assert_eq!(db.datadir.transaction_location(tx.trie_hash()).unwrap(), None);
        }
    }
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
        assert_eq!(root, block.header.state_root, "block {}", block.number);
        w.append_block(&block.header, block.hash, &block.transactions, &block.senders, &block.receipts).unwrap();
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

/// Genesis, also above block zero (set `TIGER_RETH_GENESIS_DATADIR` to a copy of a datadir that
/// `base-reth-node init` made): tiger commits the same genesis into a new
/// datadir, and every table, RocksDB row and static file must equal base-reth's byte for byte
/// (header rows after LZ4 decoding).
#[test]
fn genesis_matches_reths() {
    let Some(source) = std::env::var_os("TIGER_RETH_GENESIS_DATADIR") else { return };
    let reth = StateDb::open(Path::new(&source)).unwrap();
    let number = reth.tip().unwrap().unwrap();
    let block = read_block(&reth, number);
    let work = std::env::temp_dir().join(format!("tiger-genesis-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&work);
    let tiger = StateDb::open(&work).unwrap();
    let mut w = tiger.write().unwrap();
    let root = tiger.commit_block(&mut w, number, &block.changes).unwrap();
    assert_eq!(root, block.header.state_root);
    w.append_block(&block.header, block.hash, &block.transactions, &block.senders, &block.receipts).unwrap();
    w.commit().unwrap();
    drop(tiger);
    let tiger = StateDb::open(&work).unwrap();
    assert_eq!(snapshot(&reth, number), snapshot(&tiger, number));
    let files = |dir: &Path| {
        let mut names: Vec<_> = std::fs::read_dir(dir.join("static_files"))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .filter(|n| n != "lock")
            .collect();
        names.sort();
        names.into_iter().map(|n| (std::fs::read(dir.join("static_files").join(&n)).unwrap(), n)).collect::<Vec<_>>()
    };
    let (reth_files, tiger_files) = (files(Path::new(&source)), files(&work));
    assert_eq!(reth_files.iter().map(|f| &f.1).collect::<Vec<_>>(), tiger_files.iter().map(|f| &f.1).collect::<Vec<_>>());
    for (r, t) in reth_files.iter().zip(&tiger_files) {
        // Tiger writes LZ4 header rows uncompressed (literal-only blocks), so the header data and
        // its offsets differ; the decoded rows are compared in the snapshot.
        let headers_data = r.1.starts_with("static_file_headers_") && !r.1.ends_with(".conf");
        if !headers_data {
            assert_eq!(r.0, t.0, "{}", r.1);
        }
    }
    drop(tiger);
    let _ = std::fs::remove_dir_all(&work);
}
