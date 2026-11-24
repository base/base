//! Canonical block data in base-reth's storage v2 layout: headers, transactions, senders and
//! receipts in static files, `HeaderNumbers`, `BlockBodyIndices`, `TransactionBlocks` and
//! `ChainState` in MDBX, and `TransactionHashNumbers` in RocksDB.
//!
//! Blocks are appended inside a [`WriteTxn`], after the block's state (`StateDb::commit_block`
//! in base-tiger-trie), so the datadir commits them atomically with the state. An unwind deletes
//! the MDBX rows here; the static-file rows and transaction hashes above the new tip are cut by
//! the heal after its commit, as after a crash.

use alloy_consensus::Header;
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Address, B256, U256};
use base_common_consensus::{BaseReceipt, BaseTxEnvelope};

use crate::{
    BlockBodyIndices, Datadir, HeaderCodec, ReceiptCodec, RocksTable, Segment, StoreError, Table, TxCodec, WriteTxn,
    put_compact_u256,
};

/// reth's `ChainState` keys (`ChainStateKey`).
const LAST_FINALIZED_KEY: &[u8] = &[0];
const LAST_SAFE_KEY: &[u8] = &[1];

/// The total-difficulty column of a header row: reth's `CompactU256` (flags byte, then the
/// trimmed big-endian value).
fn difficulty_column(difficulty: U256) -> Vec<u8> {
    let mut body = Vec::with_capacity(32);
    let len = put_compact_u256(&mut body, difficulty);
    let mut out = Vec::with_capacity(33);
    out.push(len as u8);
    out.extend_from_slice(&body);
    out
}

fn le_u64(bytes: &[u8]) -> Result<u64, StoreError> {
    let bytes: [u8; 8] = bytes.try_into().map_err(|_| StoreError::Corrupt("u64 value of the wrong length".into()))?;
    Ok(u64::from_le_bytes(bytes))
}

impl WriteTxn<'_> {
    /// Appends canonical block `header` (hash `hash`) with its transactions, their senders and
    /// receipts. Blocks are appended in order; the first may have any number (genesis).
    pub fn append_block(
        &mut self,
        header: &Header,
        hash: B256,
        transactions: &[BaseTxEnvelope],
        senders: &[Address],
        receipts: &[BaseReceipt],
    ) -> Result<(), StoreError> {
        assert_eq!(transactions.len(), senders.len());
        assert_eq!(transactions.len(), receipts.len());
        let sf = &self.datadir.static_files;
        let number = header.number;
        // reth stores a zero total difficulty, except for a genesis above block zero, which it
        // writes with its own difficulty (`append_header_direct`).
        let first = sf.next_block(Segment::Headers).is_none();
        let difficulty = if first && number > 0 { header.difficulty } else { U256::ZERO };
        sf.append_header(number, &HeaderCodec::encode(header), &difficulty_column(difficulty), &hash.0)?;
        let first_tx = sf.pending_next_tx(Segment::Transactions);
        let mut rows = Vec::with_capacity(transactions.len());
        for tx in transactions {
            rows.push(TxCodec::encode(tx)?);
        }
        sf.append_txs(Segment::Transactions, number, first_tx, &rows)?;
        // A base-reth datadir pruned with `sender_recovery = "full"` has no senders segment
        // (reth does not write one): keep it that way rather than start one mid-chain.
        if sf.pending_next_tx(Segment::TransactionSenders) == first_tx {
            let rows: Vec<Vec<u8>> = senders.iter().map(|s| s.to_vec()).collect();
            sf.append_txs(Segment::TransactionSenders, number, first_tx, &rows)?;
        }
        let rows: Vec<Vec<u8>> = receipts.iter().map(ReceiptCodec::encode).collect();
        sf.append_txs(Segment::Receipts, number, first_tx, &rows)?;

        let db = &self.datadir.db;
        let (headers, bodies, tx_blocks) =
            (db.dbi(Table::HeaderNumbers), db.dbi(Table::BlockBodyIndices), db.dbi(Table::TransactionBlocks));
        let indices = BlockBodyIndices { first_tx, tx_count: transactions.len() as u64 };
        let txn = self.txn();
        txn.put(headers, hash.as_slice(), &number.to_le_bytes())?;
        txn.put(bodies, &number.to_be_bytes(), &indices.to_bytes())?;
        if indices.tx_count > 0 {
            txn.put(tx_blocks, &(indices.next_tx() - 1).to_be_bytes(), &number.to_le_bytes())?;
        }
        for (i, tx) in transactions.iter().enumerate() {
            let tx_number = first_tx + i as u64;
            self.rocks.put(RocksTable::TransactionHashNumbers, tx.trie_hash().to_vec(), tx_number.to_le_bytes().to_vec());
        }
        Ok(())
    }

    /// Records the finalized and safe block numbers (reth's `ChainState`).
    pub fn set_chain_state(&mut self, finalized: u64, safe: u64) -> Result<(), StoreError> {
        let dbi = self.datadir.db.dbi(Table::ChainState);
        self.txn().put(dbi, LAST_FINALIZED_KEY, &finalized.to_le_bytes())?;
        self.txn().put(dbi, LAST_SAFE_KEY, &safe.to_le_bytes())?;
        Ok(())
    }

    /// Deletes the MDBX block rows of every block after `number` up to `tip`. The static-file
    /// rows and transaction hashes go in the heal after the commit.
    pub fn unwind_blocks(&mut self, number: u64, tip: u64) -> Result<(), StoreError> {
        let db = &self.datadir.db;
        let (headers, bodies, tx_blocks) =
            (db.dbi(Table::HeaderNumbers), db.dbi(Table::BlockBodyIndices), db.dbi(Table::TransactionBlocks));
        for block in number + 1..=tip {
            if let Some([_, _, hash]) = self.datadir.static_files.header(block)? {
                self.txn().delete(headers, &hash, None)?;
            }
            let indices = self.txn().get(bodies, &block.to_be_bytes())?.map(BlockBodyIndices::from_bytes).transpose()?;
            if let Some(indices) = indices {
                if indices.tx_count > 0 {
                    self.txn().delete(tx_blocks, &(indices.next_tx() - 1).to_be_bytes(), None)?;
                }
                self.txn().delete(bodies, &block.to_be_bytes(), None)?;
            }
        }
        Ok(())
    }
}

impl Datadir {
    /// Canonical block `number`'s header and hash.
    pub fn header(&self, number: u64) -> Result<Option<(Header, B256)>, StoreError> {
        match self.static_files.header(number)? {
            Some([header, _, hash]) => Ok(Some((HeaderCodec::decode(&header)?, B256::from_slice(&hash)))),
            None => Ok(None),
        }
    }

    /// The number of canonical block `hash`.
    pub fn block_number(&self, hash: B256) -> Result<Option<u64>, StoreError> {
        let txn = self.db.read()?;
        txn.get(self.db.dbi(Table::HeaderNumbers), hash.as_slice())?.map(le_u64).transpose()
    }

    /// Canonical block `number`'s transaction numbers.
    pub fn body_indices(&self, number: u64) -> Result<Option<BlockBodyIndices>, StoreError> {
        let txn = self.db.read()?;
        Ok(txn.get(self.db.dbi(Table::BlockBodyIndices), &number.to_be_bytes())?.map(BlockBodyIndices::from_bytes).transpose()?)
    }

    fn rows(&self, segment: Segment, number: u64) -> Result<Option<Vec<Vec<u8>>>, StoreError> {
        let Some(indices) = self.body_indices(number)? else { return Ok(None) };
        let mut rows = Vec::with_capacity(indices.tx_count as usize);
        for tx in indices.first_tx..indices.next_tx() {
            let row = self.static_files.tx_row(segment, tx)?;
            rows.push(row.ok_or_else(|| StoreError::Corrupt(format!("{} row {tx} of block {number} is missing", segment.name())))?);
        }
        Ok(Some(rows))
    }

    /// Canonical block `number`'s transactions.
    pub fn transactions(&self, number: u64) -> Result<Option<Vec<BaseTxEnvelope>>, StoreError> {
        let Some(rows) = self.rows(Segment::Transactions, number)? else { return Ok(None) };
        Ok(Some(rows.iter().map(|row| TxCodec::decode(row)).collect::<Result<_, _>>()?))
    }

    /// Canonical block `number`'s transaction senders, `None` when the datadir does not store
    /// them (no body, or senders pruned: recover them from the signatures, as reth does).
    pub fn senders(&self, number: u64) -> Result<Option<Vec<Address>>, StoreError> {
        let Some(indices) = self.body_indices(number)? else { return Ok(None) };
        let mut senders = Vec::with_capacity(indices.tx_count as usize);
        for tx in indices.first_tx..indices.next_tx() {
            let Some(row) = self.static_files.tx_row(Segment::TransactionSenders, tx)? else { return Ok(None) };
            senders.push(Address::try_from(row.as_slice()).map_err(|_| StoreError::Corrupt("sender row".into()))?);
        }
        Ok(Some(senders))
    }

    /// Canonical block `number`'s receipts.
    pub fn receipts(&self, number: u64) -> Result<Option<Vec<BaseReceipt>>, StoreError> {
        let Some(rows) = self.rows(Segment::Receipts, number)? else { return Ok(None) };
        Ok(Some(rows.iter().map(|row| ReceiptCodec::decode(row)).collect::<Result<_, _>>()?))
    }

    /// Where canonical transaction `hash` is: block number and index in the block.
    pub fn transaction_location(&self, hash: B256) -> Result<Option<(u64, usize)>, StoreError> {
        let Some(value) = self.rocks.get(RocksTable::TransactionHashNumbers, hash.as_slice())? else { return Ok(None) };
        let tx = le_u64(&value)?;
        let txn = self.db.read()?;
        let mut cursor = txn.cursor(self.db.dbi(Table::TransactionBlocks))?;
        let Some((_, block)) = cursor.seek(&tx.to_be_bytes())? else { return Ok(None) };
        let block = le_u64(block)?;
        drop(cursor);
        drop(txn);
        let indices = self.body_indices(block)?.ok_or_else(|| StoreError::Corrupt(format!("block {block} has no body indices")))?;
        // A hash entry may outlive its block after a crash until the next heal.
        Ok((indices.first_tx..indices.next_tx()).contains(&tx).then(|| (block, (tx - indices.first_tx) as usize)))
    }

    /// The finalized and safe block numbers (reth's `ChainState`), when recorded.
    pub fn chain_state(&self) -> Result<Option<(u64, u64)>, StoreError> {
        let txn = self.db.read()?;
        let dbi = self.db.dbi(Table::ChainState);
        match (txn.get(dbi, LAST_FINALIZED_KEY)?, txn.get(dbi, LAST_SAFE_KEY)?) {
            (Some(finalized), Some(safe)) => Ok(Some((le_u64(finalized)?, le_u64(safe)?))),
            _ => Ok(None),
        }
    }
}
