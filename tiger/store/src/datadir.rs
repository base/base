//! A base-reth storage v2 datadir: `db/` (MDBX), `static_files/` and `rocksdb/`, written in the
//! order reth writes them so a crash leaves a state either client heals.
//!
//! A [`WriteTxn`] commits static files first, then RocksDB, then MDBX. MDBX holds the stage
//! checkpoints, so the MDBX commit is the commit point: static files and RocksDB may run ahead
//! of it after a crash, never behind. Opening heals the ahead part as reth's consistency checks
//! do: history index entries of blocks above the MDBX tip are removed (found through the
//! changesets still in the static files), then every static-file segment is cut back to the tip.
//! An unwind commits MDBX first and then runs the same heal, so it has the same crash
//! behavior.

use std::{
    collections::BTreeMap,
    io,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};

use alloy_eips::eip2718::Encodable2718;

use crate::{
    AccountBeforeTx, BlockBodyIndices, CodecError, Db, Fault, History, MdbxError, Rocks, RocksBatch,
    RocksTable, Rw, Segment, StageCheckpointCodec, StaticFiles, StorageBeforeTx, Table, Txn, TxCodec,
};

/// reth's stage ids, all advanced together by tiger.
pub const STAGES: [&str; 15] = [
    "Era",
    "Headers",
    "Bodies",
    "SenderRecovery",
    "Execution",
    "PruneSenderRecovery",
    "MerkleUnwind",
    "AccountHashing",
    "StorageHashing",
    "MerkleExecute",
    "TransactionLookup",
    "IndexStorageHistory",
    "IndexAccountHistory",
    "Prune",
    "Finish",
];

/// The `Metadata` key of the storage settings.
pub const STORAGE_SETTINGS_KEY: &[u8] = b"storage_settings";
/// The storage settings tiger writes and requires: storage v2.
pub const STORAGE_SETTINGS: &str = r#"{"storage_v2":true}"#;

/// A storage failure.
#[derive(Debug)]
pub enum StoreError {
    /// MDBX refused.
    Mdbx(MdbxError),
    /// File system error.
    Io(io::Error),
    /// RocksDB refused.
    Rocks(String),
    /// Stored data is inconsistent or unsupported.
    Corrupt(String),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Mdbx(err) => write!(f, "mdbx: {err}"),
            Self::Io(err) => write!(f, "io: {err}"),
            Self::Rocks(err) => write!(f, "rocksdb: {err}"),
            Self::Corrupt(text) => write!(f, "corrupt datadir: {text}"),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<MdbxError> for StoreError {
    fn from(err: MdbxError) -> Self {
        Self::Mdbx(err)
    }
}

impl From<io::Error> for StoreError {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}

impl From<rocksdb::Error> for StoreError {
    fn from(err: rocksdb::Error) -> Self {
        Self::Rocks(err.to_string())
    }
}

impl From<CodecError> for StoreError {
    fn from(err: CodecError) -> Self {
        Self::Corrupt(err.to_string())
    }
}

/// An open datadir.
#[derive(Debug)]
pub struct Datadir {
    /// The datadir path.
    pub path: PathBuf,
    /// `db/`.
    pub db: Db,
    /// `static_files/`.
    pub static_files: StaticFiles,
    /// `rocksdb/`.
    pub rocks: Rocks,
    /// A failed commit's heal failed too (such as on a full disk): the next write heals first.
    unhealed: AtomicBool,
    /// Held by the one [`WriteTxn`] from before its heal through its commit's heals. Static
    /// files hold the open transaction's pending rows in memory, and a heal reloads them from
    /// disk and rewrites their `.conf` files, so a heal must not run beside another heal or an
    /// open transaction (the persistence thread and a reorg both write). The MDBX write lock
    /// alone does not cover a heal before `begin` or after a commit or abort.
    writer: std::sync::Mutex<()>,
}

impl Datadir {
    /// Opens (creating if needed) a storage v2 datadir and heals what a crash left ahead of the
    /// MDBX commit.
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        let db = Db::open(&path.join("db"))?;
        let static_files = StaticFiles::open(&path.join("static_files"))?;
        let rocks = Rocks::open(&path.join("rocksdb"))?;
        let datadir = Self {
            path: path.to_path_buf(),
            db,
            static_files,
            rocks,
            unhealed: AtomicBool::new(false),
            writer: std::sync::Mutex::new(()),
        };
        let settings = datadir.db.read()?.get(datadir.db.dbi(Table::Metadata), STORAGE_SETTINGS_KEY)?.map(<[u8]>::to_vec);
        match settings {
            Some(bytes) => {
                let text = scale_bytes(&bytes).ok_or_else(|| StoreError::Corrupt("bad storage settings".into()))?;
                if !String::from_utf8_lossy(text).replace(' ', "").contains(r#""storage_v2":true"#) {
                    return Err(StoreError::Corrupt(format!(
                        "storage settings {} are not storage v2",
                        String::from_utf8_lossy(text)
                    )));
                }
            }
            None => {
                if datadir.tip()?.is_some() {
                    return Err(StoreError::Corrupt("datadir without storage settings (storage v1)".into()));
                }
                let mut txn = datadir.db.write()?;
                let mut value = Vec::new();
                put_scale_bytes(&mut value, STORAGE_SETTINGS.as_bytes());
                txn.put(datadir.db.dbi(Table::Metadata), STORAGE_SETTINGS_KEY, &value)?;
                txn.commit()?;
            }
        }
        datadir.heal()?;
        Ok(datadir)
    }

    /// The last committed block: the `Finish` stage checkpoint, `None` before genesis.
    pub fn tip(&self) -> Result<Option<u64>, StoreError> {
        let txn = self.db.read()?;
        Self::tip_in(&txn, &self.db)
    }

    /// The `Finish` checkpoint as `txn` sees it.
    pub fn tip_in<K>(txn: &Txn<K>, db: &Db) -> Result<Option<u64>, StoreError> {
        match txn.get(db.dbi(Table::StageCheckpoints), b"Finish")? {
            Some(bytes) => Ok(Some(StageCheckpointCodec::decode(bytes)?)),
            None => Ok(None),
        }
    }

    /// The highest block whose account or storage history base-reth's pruner removed (its
    /// `AccountHistory` and `StorageHistory` prune checkpoints), `None` when history is whole.
    /// The state after a block below it cannot be rebuilt; reth serves the state after this
    /// block and later ones.
    pub fn history_pruned_to(&self) -> Result<Option<u64>, StoreError> {
        let txn = self.db.read()?;
        let mut highest = None;
        // reth's `PruneSegment::AccountHistory` and `StorageHistory`.
        for segment in [4u8, 5] {
            if let Some(bytes) = txn.get(self.db.dbi(Table::PruneCheckpoints), &[segment])? {
                highest = highest.max(prune_checkpoint_block(bytes)?);
            }
        }
        Ok(highest)
    }

    /// Begins the write transaction.
    pub fn write(&self) -> Result<WriteTxn<'_>, StoreError> {
        let writer = self.writer.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.unhealed.load(Ordering::SeqCst) {
            self.static_files.reopen()?;
            self.heal()?;
            self.unhealed.store(false, Ordering::SeqCst);
        }
        Ok(WriteTxn {
            txn: Some(self.db.write()?),
            datadir: self,
            history: BTreeMap::new(),
            rocks: RocksBatch::default(),
            label: 0,
            heal_after: false,
            _writer: writer,
        })
    }

    /// Cuts RocksDB history and static files back to the MDBX tip.
    pub fn heal(&self) -> Result<(), StoreError> {
        let tip = self.tip()?;
        let keep = |last: Option<u64>| match (last, tip) {
            (Some(last), Some(tip)) => last > tip,
            (Some(_), None) => true,
            (None, _) => false,
        };
        // History first: it is found through the changesets about to be cut.
        let mut batch = RocksBatch::default();
        let mut prefixes: BTreeMap<(RocksTable, Vec<u8>), ()> = BTreeMap::new();
        for (segment, table) in
            [(Segment::AccountChangeSets, RocksTable::AccountsHistory), (Segment::StorageChangeSets, RocksTable::StoragesHistory)]
        {
            let Some(last) = self.static_files.last_block(segment) else { continue };
            if !keep(Some(last)) {
                continue;
            }
            let first = tip.map_or(0, |t| t + 1).max(self.static_files.first_block(segment).unwrap_or(0));
            for block in first..=last {
                for row in self.static_files.changeset(segment, block)?.unwrap_or_default() {
                    let prefix = if segment == Segment::AccountChangeSets {
                        AccountBeforeTx::from_bytes(&row)?.address.to_vec()
                    } else {
                        let row = StorageBeforeTx::from_bytes(&row)?;
                        [row.address.as_slice(), row.slot.as_slice()].concat()
                    };
                    prefixes.insert((table, prefix), ());
                }
            }
        }
        for (table, prefix) in prefixes.keys() {
            match tip {
                Some(tip) => History::unwind(&self.rocks, *table, prefix, tip, &mut batch).map_err(StoreError::Corrupt)?,
                None => {
                    for (highest, _) in History::shards(&self.rocks, *table, prefix).map_err(StoreError::Corrupt)? {
                        batch.delete(*table, History::key(prefix, highest));
                    }
                }
            }
        }
        let next_tx = match tip {
            Some(tip) => {
                let txn = self.db.read()?;
                txn.get(self.db.dbi(Table::BlockBodyIndices), &tip.to_be_bytes())?
                    .map(BlockBodyIndices::from_bytes)
                    .transpose()?
                    .map(|b| b.next_tx())
            }
            None => Some(0),
        };
        // Hash index entries of the transactions about to be cut.
        if let Some(next_tx) = next_tx
            && keep(self.static_files.last_block(Segment::Transactions))
        {
            for tx in next_tx..self.static_files.next_tx(Segment::Transactions) {
                if let Some(row) = self.static_files.tx_row(Segment::Transactions, tx)? {
                    batch.delete(RocksTable::TransactionHashNumbers, TxCodec::decode(&row)?.trie_hash().to_vec());
                }
            }
        }
        if !batch.ops.is_empty() {
            Fault::phase(tip.map_or(0, |t| t + 1), "heal-rocksdb");
            self.rocks.write(batch)?;
        }
        for segment in Segment::ALL {
            if !keep(self.static_files.last_block(segment)) {
                continue;
            }
            Fault::phase(tip.map_or(0, |t| t + 1), "heal-static-files");
            match tip {
                Some(tip) if segment.is_tx_based() => match next_tx {
                    Some(next_tx) => self.static_files.truncate(segment, tip, Some(next_tx))?,
                    // Without body indices the transaction rows cannot be matched to blocks.
                    None => return Err(StoreError::Corrupt(format!("{} ahead of block {tip} without body indices", segment.name()))),
                },
                Some(tip) => self.static_files.truncate(segment, tip, None)?,
                None => self.static_files.clear(segment)?,
            }
        }
        Ok(())
    }
}

/// The datadir's one write transaction: the MDBX transaction (the node writes its own tables
/// through `txn`), pending static-file rows, and pending RocksDB writes.
#[derive(Debug)]
pub struct WriteTxn<'d> {
    txn: Option<Txn<Rw>>,
    /// The datadir.
    pub datadir: &'d Datadir,
    /// New history blocks per `(table, prefix)`, ascending.
    history: BTreeMap<(RocksTable, Vec<u8>), Vec<u64>>,
    /// Other RocksDB writes (such as `TransactionHashNumbers`), applied after history.
    pub rocks: RocksBatch,
    /// The block named in fault-injection crash lines.
    label: u64,
    /// Heal after the MDBX commit (unwinds).
    heal_after: bool,
    /// [`Datadir::writer`], released after the commit's heals and `Drop`'s discard.
    _writer: std::sync::MutexGuard<'d, ()>,
}

impl WriteTxn<'_> {
    /// The MDBX transaction.
    pub fn txn(&mut self) -> &mut Txn<Rw> {
        self.txn.as_mut().expect("open transaction")
    }

    /// The table handles.
    pub fn db(&self) -> &Db {
        &self.datadir.db
    }

    /// Names the block this transaction commits, for fault-injection crash lines.
    pub fn set_label(&mut self, block: u64) {
        self.label = block;
    }

    /// Queues `block` into the history index of `prefix` (address, or address ++ slot).
    pub fn add_history(&mut self, table: RocksTable, prefix: Vec<u8>, block: u64) {
        let blocks = self.history.entry((table, prefix)).or_default();
        if blocks.last() != Some(&block) {
            assert!(blocks.last().is_none_or(|last| *last < block), "history blocks must ascend");
            blocks.push(block);
        }
    }

    /// Asks for a heal after the MDBX commit (an unwind's static-file and RocksDB cleanup).
    pub fn heal_after_commit(&mut self) {
        self.heal_after = true;
    }

    /// Writes every stage checkpoint at `block`.
    pub fn set_checkpoints(&mut self, block: u64) -> Result<(), StoreError> {
        let dbi = self.datadir.db.dbi(Table::StageCheckpoints);
        let value = StageCheckpointCodec::encode(block);
        for stage in STAGES {
            self.txn().put(dbi, stage.as_bytes(), &value)?;
        }
        Ok(())
    }

    /// Commits: static files, RocksDB, then MDBX (the commit point), then the heal if asked.
    pub fn commit(mut self) -> Result<(), StoreError> {
        let datadir = self.datadir;
        let committed = (|| {
            Fault::phase(self.label, "static-files");
            datadir.static_files.commit()?;
            Fault::phase(self.label, "rocksdb");
            self.write_rocks()?;
            Fault::phase(self.label, "mdbx");
            Ok(self.txn.take().expect("open transaction").commit()?)
        })();
        if let Err(error) = committed {
            // Static files and RocksDB may be ahead of MDBX, as after a crash (a full disk does
            // this): reload the static files from disk and cut them back now, as a restart would,
            // so the next write starts from the MDBX tip.
            drop(self.txn.take());
            if let Err(heal) = datadir.static_files.reopen().map_err(StoreError::from).and_then(|()| datadir.heal()) {
                datadir.unhealed.store(true, Ordering::SeqCst);
                return Err(StoreError::Corrupt(format!("{error}; healing after it: {heal}")));
            }
            return Err(error);
        }
        // The MDBX commit is the commit point: from here on the transaction committed. A failed
        // heal (a full disk) leaves static files and RocksDB ahead of the MDBX tip, as a crash
        // would, and the next write heals them first.
        if self.heal_after && datadir.heal().is_err() {
            datadir.unhealed.store(true, Ordering::SeqCst);
        }
        Ok(())
    }

    fn write_rocks(&mut self) -> Result<(), StoreError> {
        let mut batch = RocksBatch::default();
        for ((table, prefix), blocks) in std::mem::take(&mut self.history) {
            History::append(&self.datadir.rocks, table, &prefix, &blocks, &mut batch).map_err(StoreError::Corrupt)?;
        }
        batch.ops.append(&mut std::mem::take(&mut self.rocks).ops);
        self.datadir.rocks.write(batch)?;
        Ok(())
    }

    /// Simulates a crash during the commit, for crash tests: commits static files (and RocksDB
    /// when `with_rocks`) and drops the MDBX transaction. Reopen the datadir to heal.
    pub fn crash_before_mdbx(mut self, with_rocks: bool) -> Result<(), StoreError> {
        self.datadir.static_files.commit()?;
        if with_rocks {
            self.write_rocks()?;
        }
        drop(self.txn.take());
        Ok(())
    }
}

impl Drop for WriteTxn<'_> {
    fn drop(&mut self) {
        if self.txn.is_some() {
            self.datadir.static_files.discard();
        }
    }
}

/// The `block_number` of reth's compact `PruneCheckpoint`: a flags byte (bit 0: block number
/// present), then a one-byte length and the big-endian number.
fn prune_checkpoint_block(bytes: &[u8]) -> Result<Option<u64>, StoreError> {
    let bad = || StoreError::Corrupt("bad prune checkpoint".into());
    let (&flags, rest) = bytes.split_first().ok_or_else(bad)?;
    if flags & 1 == 0 {
        return Ok(None);
    }
    let (&len, rest) = rest.split_first().ok_or_else(bad)?;
    let number = rest.get(..len as usize).filter(|n| n.len() <= 8).ok_or_else(bad)?;
    Ok(Some(number.iter().fold(0u64, |acc, b| (acc << 8) | u64::from(*b))))
}

/// The bytes of a SCALE-encoded `Vec<u8>` (compact length prefix), if well formed.
pub fn scale_bytes(bytes: &[u8]) -> Option<&[u8]> {
    let first = *bytes.first()?;
    let (len, skip) = match first & 3 {
        0 => ((first >> 2) as usize, 1),
        1 => ((u16::from_le_bytes([first, *bytes.get(1)?]) >> 2) as usize, 2),
        2 => ((u32::from_le_bytes(bytes.get(..4)?.try_into().ok()?) >> 2) as usize, 4),
        _ => return None,
    };
    let body = bytes.get(skip..)?;
    (body.len() == len).then_some(body)
}

/// Appends `data` SCALE-encoded as a `Vec<u8>`.
pub fn put_scale_bytes(out: &mut Vec<u8>, data: &[u8]) {
    let len = data.len();
    assert!(len < 1 << 30, "SCALE big-integer lengths are not needed");
    if len < 1 << 6 {
        out.push((len as u8) << 2);
    } else if len < 1 << 14 {
        out.extend_from_slice(&(((len as u16) << 2) | 1).to_le_bytes());
    } else {
        out.extend_from_slice(&(((len as u32) << 2) | 2).to_le_bytes());
    }
    out.extend_from_slice(data);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scale_bytes_round_trip() {
        for len in [0usize, 1, 19, 63, 64, 300, 16_383, 16_384, 70_000] {
            let data = vec![7u8; len];
            let mut out = Vec::new();
            put_scale_bytes(&mut out, &data);
            assert_eq!(scale_bytes(&out), Some(&data[..]));
        }
        let mut out = Vec::new();
        put_scale_bytes(&mut out, STORAGE_SETTINGS.as_bytes());
        assert_eq!(out[0], 0x4c, "reth's fixture starts with L");
    }

    /// Checkpoints copied from a Base mainnet base-reth full node's `PruneCheckpoints`.
    #[test]
    fn prune_checkpoint_block_numbers() {
        // AccountHistory: block 47859141, Distance(14000000).
        assert_eq!(prune_checkpoint_block(&hex("010402da45c501d59f80")).unwrap(), Some(47_859_141));
        // Receipts: block 47999999 with a transaction number, Distance(13392000).
        assert_eq!(prune_checkpoint_block(&hex("030402dc6bff0501981db2ca01cc5880")).unwrap(), Some(47_999_999));
        // No block pruned yet, Full.
        assert_eq!(prune_checkpoint_block(&[0, 0]).unwrap(), None);
        assert!(prune_checkpoint_block(&hex("0104")).is_err());
    }

    fn hex(text: &str) -> Vec<u8> {
        (0..text.len()).step_by(2).map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap()).collect()
    }

    /// A heal owed from a failed commit (a full disk) reloads the static files from disk, which
    /// would drop the rows another thread's open transaction has pending: the second writer waits
    /// for the first to finish before it heals (state fuzz T9).
    #[test]
    fn a_heal_waits_for_the_open_transaction() {
        let path = std::env::temp_dir().join(format!("tiger-heal-waits-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        let datadir = Datadir::open(&path).unwrap();
        let mut w = datadir.write().unwrap();
        datadir.static_files.append_changeset(Segment::AccountChangeSets, 0, &[]).unwrap();
        w.set_checkpoints(0).unwrap();
        datadir.unhealed.store(true, Ordering::SeqCst);
        std::thread::scope(|scope| {
            let other = scope.spawn(|| drop(datadir.write().unwrap()));
            std::thread::sleep(std::time::Duration::from_millis(200));
            w.commit().unwrap();
            other.join().unwrap();
        });
        assert_eq!(datadir.tip().unwrap(), Some(0));
        assert_eq!(datadir.static_files.last_block(Segment::AccountChangeSets), Some(0));
        drop(datadir);
        std::fs::remove_dir_all(&path).unwrap();
    }

    /// Opens a base-reth datadir (set `TIGER_RETH_DATADIR`; it is healed, so use a copy).
    #[test]
    fn opens_a_reth_datadir() {
        let Ok(path) = std::env::var("TIGER_RETH_DATADIR") else { return };
        let datadir = Datadir::open(Path::new(&path)).unwrap();
        let tip = datadir.tip().unwrap().unwrap();
        assert_eq!(datadir.static_files.last_block(Segment::Headers), Some(tip));
        assert_eq!(datadir.static_files.last_block(Segment::AccountChangeSets), Some(tip));
    }
}
