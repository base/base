//! The node's block tree, canonical chain and state.
//!
//! State lives in the state database (`base-tiger-trie`), a base-reth storage v2 datadir under
//! `--datadir`. As in base-reth's engine tree, a validated block stays in memory with its state
//! changes until it becomes canonical; making a branch canonical unwinds the database to the
//! fork point and commits the branch's blocks, with their block records and the forkchoice heads,
//! in one write transaction, so the database tip is always the canonical head. Side-branch state
//! is read as a database view at the fork point with the branch's changes on top. Blocks that
//! were never canonical are not persisted: after a restart the consensus node sends them again.
//!
//! Block records (header, transactions, senders, receipts) are kept in tiger's own
//! `TigerBlocks` table until the node writes reth's static files.

use std::{
    collections::HashMap,
    path::Path,
    sync::Arc,
};

use alloy_consensus::Header;
use alloy_eips::eip2718::{Decodable2718, Encodable2718};
use alloy_primitives::{Address, B256, Bytes};
use alloy_rlp::{RlpDecodable, RlpEncodable};
use base_common_consensus::{BaseReceiptEnvelope, BaseTxEnvelope as SignedTx};
use base_tiger_store::{Fault, Table, WriteTxn};
use base_tiger_trie::{BlockChanges, StateDb};

use crate::{Executed, Genesis, NodeState, block_changes, genesis::genesis_changes};

/// Blocks `BLOCKHASH` can reach.
const BLOCK_HASH_WINDOW: u64 = 256;
/// The `TigerForkchoice` key of the forkchoice heads.
const FORKCHOICE_KEY: &[u8] = b"forkchoice";

/// One stored block with its execution results.
#[derive(Debug, Clone)]
pub struct StoredBlock {
    /// The header.
    pub header: Header,
    /// The block hash.
    pub hash: B256,
    /// Transactions, in block order.
    pub transactions: Vec<SignedTx>,
    /// Transaction hashes, in block order.
    pub tx_hashes: Vec<B256>,
    /// Sender of each transaction.
    pub senders: Vec<Address>,
    /// Receipt of each transaction.
    pub receipts: Vec<BaseReceiptEnvelope>,
    /// The block's state changes while the block is held in memory (`None` for blocks loaded
    /// from the database, whose changes are its changesets).
    pub changes: Option<Arc<BlockChanges>>,
}

impl StoredBlock {
    fn new(
        header: Header,
        hash: B256,
        transactions: Vec<SignedTx>,
        senders: Vec<Address>,
        receipts: Vec<BaseReceiptEnvelope>,
        changes: Option<Arc<BlockChanges>>,
    ) -> Self {
        assert_eq!(transactions.len(), senders.len());
        assert_eq!(transactions.len(), receipts.len());
        let tx_hashes = transactions.iter().map(|tx| tx.trie_hash()).collect();
        Self { header, hash, transactions, tx_hashes, senders, receipts, changes }
    }

    /// The block number.
    pub const fn number(&self) -> u64 {
        self.header.number
    }

    fn record(&self) -> Vec<u8> {
        alloy_rlp::encode(BlockRecord {
            header: self.header.clone(),
            transactions: self.transactions.iter().map(|tx| tx.encoded_2718().into()).collect(),
            senders: self.senders.clone(),
            receipts: self.receipts.iter().map(|receipt| receipt.encoded_2718().into()).collect(),
        })
    }

    fn from_record(payload: &[u8]) -> Result<Self, String> {
        let record: BlockRecord =
            alloy_rlp::decode_exact(payload).map_err(|error| format!("block record: {error}"))?;
        let transactions = record
            .transactions
            .iter()
            .map(|raw| SignedTx::decode_2718_exact(raw))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| format!("block record transaction: {error}"))?;
        let receipts = record
            .receipts
            .iter()
            .map(|raw| BaseReceiptEnvelope::decode_2718_exact(raw))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| format!("block record receipt: {error}"))?;
        let hash = record.header.hash_slow();
        Ok(Self::new(record.header, hash, transactions, record.senders, receipts, None))
    }
}

/// The three heads the consensus node sets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, RlpEncodable, RlpDecodable)]
pub struct Forkchoice {
    /// The canonical head.
    pub head: B256,
    /// The safe head.
    pub safe: B256,
    /// The finalized head.
    pub finalized: B256,
}

/// A stored block as recorded in `TigerBlocks`.
#[derive(Debug, RlpEncodable, RlpDecodable)]
struct BlockRecord {
    header: Header,
    transactions: Vec<Bytes>,
    senders: Vec<Address>,
    receipts: Vec<Bytes>,
}

/// The block tree, the canonical chain, and the forkchoice heads.
#[derive(Debug)]
pub struct Chain {
    /// The chain and its genesis.
    pub genesis: Genesis,
    /// The state database.
    pub db: StateDb,
    blocks: HashMap<B256, Arc<StoredBlock>>,
    /// Canonical block hashes, indexed by number minus the genesis number.
    canonical: Vec<B256>,
    /// Canonical transactions by hash: block number and index.
    tx_index: HashMap<B256, (u64, usize)>,
    forkchoice: Forkchoice,
}

impl Chain {
    /// Opens (or initializes) the chain in the datadir `datadir`.
    pub fn open(datadir: &Path, genesis: Genesis) -> Result<Self, String> {
        Fault::init();
        if genesis.header.number != 0 {
            return Err(format!(
                "genesis block {} is above zero; the state database does not support that yet",
                genesis.header.number
            ));
        }
        let db = StateDb::open(datadir).map_err(|error| format!("opening {}: {error}", datadir.display()))?;
        let mut chain = Self {
            db,
            blocks: HashMap::new(),
            canonical: Vec::new(),
            tx_index: HashMap::new(),
            forkchoice: Forkchoice { head: genesis.hash, safe: genesis.hash, finalized: genesis.hash },
            genesis,
        };
        let tip = chain.db.tip().map_err(|error| error.to_string())?;
        let Some(tip) = tip else {
            let alloc = chain.genesis.alloc.clone().ok_or(
                "a new datadir for a built-in chain needs a genesis file with its full allocation",
            )?;
            let block = Arc::new(StoredBlock::new(
                chain.genesis.header.clone(),
                chain.genesis.hash,
                Vec::new(),
                Vec::new(),
                Vec::new(),
                None,
            ));
            let mut w = chain.db.write().map_err(|error| error.to_string())?;
            let root = chain.db.commit_block(&mut w, 0, &genesis_changes(&alloc)).map_err(|error| error.to_string())?;
            if root != chain.genesis.header.state_root {
                return Err(format!("genesis state root {root} is not {}", chain.genesis.header.state_root));
            }
            Self::put_block(&mut w, &block)?;
            Self::put_forkchoice(&mut w, chain.forkchoice)?;
            w.commit().map_err(|error| error.to_string())?;
            chain.canonical.push(block.hash);
            chain.blocks.insert(block.hash, block);
            return Ok(chain);
        };
        {
            let txn = chain.db.datadir.db.read().map_err(|error| error.to_string())?;
            let blocks_dbi = chain.db.datadir.db.dbi(Table::TigerBlocks);
            for number in 0..=tip {
                let record = txn
                    .get(blocks_dbi, &number.to_be_bytes())
                    .map_err(|error| error.to_string())?
                    .ok_or_else(|| format!("datadir has state up to block {tip} but no block record {number}"))?;
                let block = Arc::new(StoredBlock::from_record(record)?);
                if number == 0 && block.hash != chain.genesis.hash {
                    return Err(format!("datadir holds another chain (genesis {})", block.hash));
                }
                for (index, hash) in block.tx_hashes.iter().enumerate() {
                    chain.tx_index.insert(*hash, (number, index));
                }
                chain.canonical.push(block.hash);
                chain.blocks.insert(block.hash, block);
            }
            if let Some(bytes) = txn
                .get(chain.db.datadir.db.dbi(Table::TigerForkchoice), FORKCHOICE_KEY)
                .map_err(|error| error.to_string())?
            {
                chain.forkchoice =
                    alloy_rlp::decode_exact(bytes).map_err(|error| format!("forkchoice record: {error}"))?;
            }
        }
        let head = *chain.canonical.last().expect("genesis is canonical");
        if chain.forkchoice.head != head {
            return Err(format!("forkchoice head {} is not the stored tip {head}", chain.forkchoice.head));
        }
        Ok(chain)
    }

    fn put_block(w: &mut WriteTxn<'_>, block: &StoredBlock) -> Result<(), String> {
        let dbi = w.db().dbi(Table::TigerBlocks);
        w.txn().put(dbi, &block.number().to_be_bytes(), &block.record()).map_err(|error| error.to_string())
    }

    fn put_forkchoice(w: &mut WriteTxn<'_>, forkchoice: Forkchoice) -> Result<(), String> {
        let dbi = w.db().dbi(Table::TigerForkchoice);
        w.txn().put(dbi, FORKCHOICE_KEY, &alloy_rlp::encode(forkchoice)).map_err(|error| error.to_string())
    }

    /// The block with hash `hash`.
    pub fn block(&self, hash: &B256) -> Option<Arc<StoredBlock>> {
        self.blocks.get(hash).cloned()
    }

    /// The canonical block hash at `number`.
    pub fn canonical_hash(&self, number: u64) -> Option<B256> {
        let index = number.checked_sub(self.genesis.header.number)?;
        self.canonical.get(usize::try_from(index).ok()?).copied()
    }

    /// The canonical block at `number`.
    pub fn canonical_block(&self, number: u64) -> Option<Arc<StoredBlock>> {
        self.canonical_hash(number).and_then(|hash| self.block(&hash))
    }

    /// Whether `hash` is on the canonical chain.
    pub fn is_canonical(&self, hash: &B256) -> bool {
        self.block(hash).is_some_and(|block| self.canonical_hash(block.number()) == Some(*hash))
    }

    /// The canonical head.
    pub fn head(&self) -> Arc<StoredBlock> {
        self.block(self.canonical.last().expect("genesis is canonical")).expect("canonical blocks are stored")
    }

    /// The forkchoice heads.
    pub const fn forkchoice(&self) -> Forkchoice {
        self.forkchoice
    }

    /// Where the canonical transaction `hash` is: block number and index.
    pub fn transaction(&self, hash: &B256) -> Option<(u64, usize)> {
        self.tx_index.get(hash).copied()
    }

    /// The state after `block` (canonical or a side branch) with the block hashes `BLOCKHASH`
    /// reads in a child of it. Holds a database read transaction: use it and drop it.
    pub fn state_after(&self, block: &StoredBlock) -> Result<NodeState<'_>, String> {
        let mut overlays = Vec::new();
        let mut at = block;
        while !self.is_canonical(&at.hash) {
            let changes = at.changes.as_ref().ok_or_else(|| format!("state of block {} is gone", at.hash))?;
            overlays.push(BlockChanges::clone(changes));
            at = self.blocks.get(&at.header.parent_hash).ok_or_else(|| format!("block {} has no parent", at.hash))?;
        }
        overlays.reverse();
        let view = self.db.view(at.number(), overlays).map_err(|error| error.to_string())?;
        Ok(NodeState::new(view, self.block_hashes(block)))
    }

    /// The hashes of `block` and its ancestors in the `BLOCKHASH` window, zero below genesis.
    fn block_hashes(&self, block: &StoredBlock) -> HashMap<u64, B256> {
        let mut hashes = HashMap::with_capacity(BLOCK_HASH_WINDOW as usize);
        let mut at = Some(block);
        while let Some(current) = at {
            if hashes.len() as u64 >= BLOCK_HASH_WINDOW {
                break;
            }
            hashes.insert(current.number(), current.hash);
            at = self.blocks.get(&current.header.parent_hash).map(Arc::as_ref);
        }
        let first = block.number().saturating_sub(BLOCK_HASH_WINDOW);
        for number in first..self.genesis.header.number.min(block.number()) {
            hashes.insert(number, B256::ZERO);
        }
        hashes
    }

    /// Keeps a validated block in memory until it becomes canonical.
    pub fn insert(&mut self, hash: B256, executed: &Executed) -> Result<Arc<StoredBlock>, String> {
        if let Some(block) = self.blocks.get(&hash) {
            return Ok(Arc::clone(block));
        }
        assert!(self.blocks.contains_key(&executed.header.parent_hash), "parent is stored");
        let block = Arc::new(StoredBlock::new(
            executed.header.clone(),
            hash,
            executed.transactions.clone(),
            executed.senders.clone(),
            executed.receipts.clone(),
            Some(Arc::new(block_changes(&executed.changes))),
        ));
        self.blocks.insert(hash, Arc::clone(&block));
        Ok(block)
    }

    /// Makes `head` (a stored block) the canonical head: unwinds the database to the fork point
    /// and commits the new branch, its block records and the head in one transaction.
    pub fn set_canonical(&mut self, head: B256) -> Result<(), String> {
        let genesis_number = self.genesis.header.number;
        let mut branch = Vec::new();
        let mut at = head;
        loop {
            let block = self.blocks.get(&at).ok_or_else(|| format!("block {at} is not stored"))?;
            if self.canonical_hash(block.number()) == Some(at) {
                break;
            }
            if block.number() == genesis_number {
                return Err(format!("block {head} does not descend from the genesis block"));
            }
            branch.push(Arc::clone(block));
            at = block.header.parent_hash;
        }
        let fork_number = self.blocks[&at].number();
        let tip = self.head().number();
        if branch.is_empty() && fork_number == tip {
            return Ok(());
        }
        branch.reverse();
        if fork_number < tip {
            // Unwinding cuts the static files after its commit, so it commits on its own, with
            // the fork point as the persisted head.
            let mut w = self.db.write().map_err(|error| error.to_string())?;
            self.db.unwind_in(&mut w, fork_number).map_err(|error| error.to_string())?;
            let dbi = w.db().dbi(Table::TigerBlocks);
            for number in fork_number + 1..=tip {
                w.txn().delete(dbi, &number.to_be_bytes(), None).map_err(|error| error.to_string())?;
            }
            Self::put_forkchoice(&mut w, Forkchoice { head: at, ..self.forkchoice })?;
            w.commit().map_err(|error| error.to_string())?;
        }
        let mut w = self.db.write().map_err(|error| error.to_string())?;
        for block in &branch {
            let changes = block.changes.as_ref().ok_or_else(|| format!("state of block {} is gone", block.hash))?;
            let root = self.db.commit_block(&mut w, block.number(), changes).map_err(|error| error.to_string())?;
            if root != block.header.state_root {
                return Err(format!(
                    "committing block {} {} gave state root {root}, not {}",
                    block.number(),
                    block.hash,
                    block.header.state_root
                ));
            }
            Self::put_block(&mut w, block)?;
        }
        Self::put_forkchoice(&mut w, Forkchoice { head, ..self.forkchoice })?;
        w.commit().map_err(|error| error.to_string())?;

        // The unwound blocks stay readable as a side branch only while their changes are in
        // memory; blocks loaded from the database lose theirs and are dropped.
        let keep = (fork_number - genesis_number) as usize + 1;
        for hash in self.canonical.drain(keep..) {
            let block = &self.blocks[&hash];
            for tx in &block.tx_hashes {
                self.tx_index.remove(tx);
            }
            if block.changes.is_none() {
                self.blocks.remove(&hash);
            }
        }
        for block in branch {
            for (index, tx) in block.tx_hashes.iter().enumerate() {
                self.tx_index.insert(*tx, (block.number(), index));
            }
            self.canonical.push(block.hash);
        }
        self.forkchoice.head = head;
        assert_eq!(self.canonical.last(), Some(&head));
        Ok(())
    }

    /// Applies a forkchoice update whose blocks are all stored, and persists it. `safe` and
    /// `finalized` must be canonical after the head moves (zero hashes leave them unchanged).
    pub fn update_forkchoice(&mut self, update: Forkchoice) -> Result<(), String> {
        self.set_canonical(update.head)?;
        let mut next = Forkchoice { head: update.head, ..self.forkchoice };
        if !update.safe.is_zero() {
            next.safe = update.safe;
        }
        if !update.finalized.is_zero() {
            next.finalized = update.finalized;
        }
        if next != self.forkchoice {
            let mut w = self.db.write().map_err(|error| error.to_string())?;
            Self::put_forkchoice(&mut w, next)?;
            w.commit().map_err(|error| error.to_string())?;
            self.forkchoice = next;
        }
        Ok(())
    }
}
