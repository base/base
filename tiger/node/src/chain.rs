//! The node's block tree, canonical chain and state.
//!
//! Everything canonical lives in the datadir (`base-tiger-store` and `base-tiger-trie`), a
//! base-reth storage v2 datadir under `--datadir`: state, and block data in reth's layout
//! (headers, transactions, senders and receipts in static files, with reth's indexes), read on
//! demand. As in base-reth's engine tree, a validated block stays in memory with its state
//! changes until it becomes canonical; making a branch canonical unwinds the datadir to the fork
//! point and commits the branch's state and blocks, with the forkchoice heads, in one write
//! transaction, so the datadir tip is always the canonical head. Side-branch state is read as a
//! datadir view at the fork point with the branch's changes on top. Blocks stay in memory while
//! they are within `MEMORY_DEPTH` of the head, so a shallow reorg back onto an unwound branch
//! needs no download; older and never-canonical blocks are not persisted, and after a restart
//! the consensus node sends them again.

use std::{collections::HashMap, path::Path, sync::Arc};

use alloy_consensus::{Header, TxReceipt};
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Address, B256};
use alloy_rlp::{RlpDecodable, RlpEncodable};
use base_common_consensus::{BaseReceipt, BaseReceiptEnvelope, BaseTxEnvelope as SignedTx};
use base_tiger_store::{Fault, Table, WriteTxn};
use base_tiger_trie::{BlockChanges, StateDb};

use crate::{Executed, Genesis, NodeState, block_changes, genesis::genesis_changes};

/// Blocks `BLOCKHASH` can reach.
const BLOCK_HASH_WINDOW: u64 = 256;
/// Blocks this far below the head or deeper leave memory (base-reth's engine tree keeps a
/// similar window of executed blocks in memory).
const MEMORY_DEPTH: u64 = 64;
/// The `TigerForkchoice` key of the forkchoice heads.
const FORKCHOICE_KEY: &[u8] = b"forkchoice";

/// One block with its execution results.
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
    /// The block's state changes while the block is held in memory (`None` for blocks read
    /// from the datadir, whose changes are its changesets).
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

    /// Appends the block to the datadir's canonical block data.
    fn append(&self, w: &mut WriteTxn<'_>) -> Result<(), String> {
        let receipts: Vec<BaseReceipt> = self.receipts.iter().cloned().map(BaseReceipt::from).collect();
        w.append_block(&self.header, self.hash, &self.transactions, &self.senders, &receipts)
            .map_err(|error| format!("appending block {}: {error}", self.number()))
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

/// The block tree, the canonical chain, and the forkchoice heads.
#[derive(Debug)]
pub struct Chain {
    /// The chain and its genesis.
    pub genesis: Genesis,
    /// The state database.
    pub db: StateDb,
    /// Executed blocks within `MEMORY_DEPTH` of the head, canonical or not, with their changes.
    blocks: HashMap<B256, Arc<StoredBlock>>,
    head: Arc<StoredBlock>,
    forkchoice: Forkchoice,
}

impl Chain {
    /// Opens (or initializes) the chain in the datadir `datadir`.
    pub fn open(datadir: &Path, genesis: Genesis) -> Result<Self, String> {
        Fault::init();
        let db = StateDb::open(datadir).map_err(|error| format!("opening {}: {error}", datadir.display()))?;
        let genesis_block = Arc::new(StoredBlock::new(
            genesis.header.clone(),
            genesis.hash,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            None,
        ));
        let mut chain = Self {
            db,
            blocks: HashMap::new(),
            head: Arc::clone(&genesis_block),
            forkchoice: Forkchoice { head: genesis.hash, safe: genesis.hash, finalized: genesis.hash },
            genesis,
        };
        let genesis_number = chain.genesis.header.number;
        let tip = chain.db.tip().map_err(|error| error.to_string())?;
        let Some(tip) = tip else {
            let alloc = chain.genesis.alloc.clone().ok_or(
                "a new datadir for a built-in chain needs a genesis file with its full allocation",
            )?;
            let mut w = chain.db.write().map_err(|error| error.to_string())?;
            let root = chain
                .db
                .commit_block(&mut w, genesis_number, &genesis_changes(&alloc))
                .map_err(|error| error.to_string())?;
            if root != chain.genesis.header.state_root {
                return Err(format!("genesis state root {root} is not {}", chain.genesis.header.state_root));
            }
            genesis_block.append(&mut w)?;
            w.set_chain_state(genesis_number, genesis_number).map_err(|error| error.to_string())?;
            Self::put_forkchoice(&mut w, chain.forkchoice)?;
            w.commit().map_err(|error| error.to_string())?;
            return Ok(chain);
        };
        match chain.db.datadir.header(genesis_number).map_err(|error| error.to_string())? {
            Some((_, hash)) if hash == chain.genesis.hash => {}
            Some((_, hash)) => return Err(format!("datadir holds another chain (genesis {hash})")),
            None => return Err(format!("datadir has no genesis block {genesis_number}")),
        }
        chain.head = Arc::new(chain.load(tip)?.ok_or_else(|| format!("datadir has state up to block {tip} but no block {tip}"))?);
        {
            let txn = chain.db.datadir.db.read().map_err(|error| error.to_string())?;
            if let Some(bytes) = txn
                .get(chain.db.datadir.db.dbi(Table::TigerForkchoice), FORKCHOICE_KEY)
                .map_err(|error| error.to_string())?
            {
                chain.forkchoice =
                    alloy_rlp::decode_exact(bytes).map_err(|error| format!("forkchoice record: {error}"))?;
            }
        }
        if chain.forkchoice.head != chain.head.hash {
            return Err(format!("forkchoice head {} is not the stored tip {}", chain.forkchoice.head, chain.head.hash));
        }
        Ok(chain)
    }

    /// Reads canonical block `number` from the datadir.
    fn load(&self, number: u64) -> Result<Option<StoredBlock>, String> {
        let datadir = &self.db.datadir;
        let read = || -> Result<Option<StoredBlock>, base_tiger_store::StoreError> {
            let Some((header, hash)) = datadir.header(number)? else { return Ok(None) };
            let missing = || base_tiger_store::StoreError::Corrupt(format!("block {number} has a header but no body"));
            let transactions = datadir.transactions(number)?.ok_or_else(missing)?;
            let senders = datadir.senders(number)?.ok_or_else(missing)?;
            let receipts = datadir.receipts(number)?.ok_or_else(missing)?;
            let receipts = receipts.into_iter().map(|receipt| receipt.into_with_bloom().into()).collect();
            Ok(Some(StoredBlock::new(header, hash, transactions, senders, receipts, None)))
        };
        read().map_err(|error| format!("reading block {number}: {error}"))
    }

    /// Reads canonical block `number`, which the datadir must hold.
    fn load_canonical(&self, number: u64) -> Arc<StoredBlock> {
        match self.load(number) {
            Ok(Some(block)) => Arc::new(block),
            Ok(None) => panic!("canonical block {number} is not in the datadir"),
            Err(error) => panic!("{error}"),
        }
    }

    fn put_forkchoice(w: &mut WriteTxn<'_>, forkchoice: Forkchoice) -> Result<(), String> {
        let dbi = w.db().dbi(Table::TigerForkchoice);
        w.txn().put(dbi, FORKCHOICE_KEY, &alloy_rlp::encode(forkchoice)).map_err(|error| error.to_string())
    }

    /// The block with hash `hash`: in memory, or canonical in the datadir.
    pub fn block(&self, hash: &B256) -> Option<Arc<StoredBlock>> {
        if let Some(block) = self.blocks.get(hash) {
            return Some(Arc::clone(block));
        }
        if *hash == self.head.hash {
            return Some(Arc::clone(&self.head));
        }
        let number = self.db.datadir.block_number(*hash).unwrap_or_else(|error| panic!("reading block {hash}: {error}"))?;
        Some(self.load_canonical(number))
    }

    /// The canonical block hash at `number`.
    pub fn canonical_hash(&self, number: u64) -> Option<B256> {
        if number < self.genesis.header.number || number > self.head.number() {
            return None;
        }
        if number == self.head.number() {
            return Some(self.head.hash);
        }
        let header = self.db.datadir.header(number).unwrap_or_else(|error| panic!("reading header {number}: {error}"));
        Some(header.unwrap_or_else(|| panic!("canonical header {number} is not in the datadir")).1)
    }

    /// The canonical block at `number`.
    pub fn canonical_block(&self, number: u64) -> Option<Arc<StoredBlock>> {
        let hash = self.canonical_hash(number)?;
        Some(self.blocks.get(&hash).cloned().unwrap_or_else(|| {
            if hash == self.head.hash { Arc::clone(&self.head) } else { self.load_canonical(number) }
        }))
    }

    /// Whether `hash` is on the canonical chain.
    pub fn is_canonical(&self, hash: &B256) -> bool {
        match self.blocks.get(hash) {
            Some(block) => self.canonical_hash(block.number()) == Some(*hash),
            None => {
                *hash == self.head.hash
                    || self.db.datadir.block_number(*hash).unwrap_or_else(|error| panic!("reading block {hash}: {error}")).is_some()
            }
        }
    }

    /// The canonical head.
    pub fn head(&self) -> Arc<StoredBlock> {
        Arc::clone(&self.head)
    }

    /// The forkchoice heads.
    pub const fn forkchoice(&self) -> Forkchoice {
        self.forkchoice
    }

    /// Where the canonical transaction `hash` is: block number and index.
    pub fn transaction(&self, hash: &B256) -> Option<(u64, usize)> {
        let location =
            self.db.datadir.transaction_location(*hash).unwrap_or_else(|error| panic!("reading transaction {hash}: {error}"))?;
        (location.0 <= self.head.number()).then_some(location)
    }

    /// The state after `block` (canonical or a side branch) with the block hashes `BLOCKHASH`
    /// reads in a child of it. Holds a database read transaction: use it and drop it.
    pub fn state_after(&self, block: &StoredBlock) -> Result<NodeState<'_>, String> {
        let mut overlays = Vec::new();
        let mut number = block.number();
        if !self.is_canonical(&block.hash) {
            let (mut hash, mut changes, mut parent) = (block.hash, block.changes.clone(), block.header.parent_hash);
            loop {
                let state = changes.ok_or_else(|| format!("state of block {hash} is gone"))?;
                overlays.push(BlockChanges::clone(&state));
                let at = self.block(&parent).ok_or_else(|| format!("block {hash} has no stored parent"))?;
                if self.is_canonical(&at.hash) {
                    number = at.number();
                    break;
                }
                (hash, changes, parent) = (at.hash, at.changes.clone(), at.header.parent_hash);
            }
        }
        overlays.reverse();
        let view = self.db.view(number, overlays).map_err(|error| error.to_string())?;
        Ok(NodeState::new(view, self.block_hashes(block)))
    }

    /// The hashes of `block` and its ancestors in the `BLOCKHASH` window, zero below genesis.
    fn block_hashes(&self, block: &StoredBlock) -> HashMap<u64, B256> {
        let first = block.number().saturating_sub(BLOCK_HASH_WINDOW - 1);
        let mut hashes = HashMap::with_capacity(BLOCK_HASH_WINDOW as usize);
        let mut number = block.number();
        let mut hash = block.hash;
        // Walk the in-memory side branch down to the canonical chain, then read canonical hashes.
        loop {
            hashes.insert(number, hash);
            if number <= first || self.canonical_hash(number) == Some(hash) {
                break;
            }
            match self.blocks.get(&hash) {
                Some(at) => {
                    hash = at.header.parent_hash;
                    number -= 1;
                }
                // `block` itself may be outside memory (being executed): its parent is next.
                None if number == block.number() => {
                    hash = block.header.parent_hash;
                    number -= 1;
                }
                None => break,
            }
        }
        for below in first..number {
            hashes.insert(below, self.canonical_hash(below).unwrap_or(B256::ZERO));
        }
        hashes
    }

    /// Keeps a validated block in memory until it becomes canonical.
    pub fn insert(&mut self, hash: B256, executed: &Executed) -> Result<Arc<StoredBlock>, String> {
        if let Some(block) = self.blocks.get(&hash) {
            return Ok(Arc::clone(block));
        }
        assert!(self.block(&executed.header.parent_hash).is_some(), "parent is stored");
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

    /// Makes `head` (a stored block) the canonical head: unwinds the datadir to the fork point
    /// and commits the new branch's state and blocks and the head in one transaction.
    pub fn set_canonical(&mut self, head: B256) -> Result<(), String> {
        let genesis_number = self.genesis.header.number;
        let mut branch = Vec::new();
        let mut at = head;
        let fork_number = loop {
            let block = self.block(&at).ok_or_else(|| format!("block {at} is not stored"))?;
            if self.canonical_hash(block.number()) == Some(at) {
                break block.number();
            }
            if block.number() == genesis_number {
                return Err(format!("block {head} does not descend from the genesis block"));
            }
            at = block.header.parent_hash;
            branch.push(block);
        };
        let tip = self.head.number();
        if branch.is_empty() && fork_number == tip {
            return Ok(());
        }
        branch.reverse();
        let fork_block = self.canonical_block(fork_number).expect("the fork point is canonical");
        if fork_number < tip {
            // Unwinding cuts the static files after its commit, so it commits on its own, with
            // the fork point as the persisted head.
            let mut w = self.db.write().map_err(|error| error.to_string())?;
            self.db.unwind_in(&mut w, fork_number).map_err(|error| error.to_string())?;
            Self::put_forkchoice(&mut w, Forkchoice { head: at, ..self.forkchoice })?;
            w.commit().map_err(|error| error.to_string())?;
            self.head = fork_block;
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
            block.append(&mut w)?;
        }
        Self::put_forkchoice(&mut w, Forkchoice { head, ..self.forkchoice })?;
        w.commit().map_err(|error| error.to_string())?;
        self.head = branch.pop().expect("the branch is not empty");
        self.forkchoice.head = head;

        self.prune();
        Ok(())
    }

    /// Drops blocks `MEMORY_DEPTH` or more below the head, and blocks whose state is no longer
    /// reachable: a side block must descend, through blocks in memory, from a canonical block.
    /// (An unwound block that was read from the datadir has no changes in memory, so its
    /// descendants go too, and report as unknown until they are sent again.)
    fn prune(&mut self) {
        let low = self.head.number().saturating_sub(MEMORY_DEPTH);
        let mut kept: Vec<Arc<StoredBlock>> =
            self.blocks.values().filter(|block| block.number() > low).cloned().collect();
        kept.sort_by_key(|block| block.number());
        let mut reachable = HashMap::with_capacity(kept.len());
        for block in kept {
            let parent = block.header.parent_hash;
            if reachable.contains_key(&parent) || self.is_canonical(&block.hash) || self.is_canonical(&parent) {
                reachable.insert(block.hash, block);
            }
        }
        self.blocks = reachable;
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
            let number = |hash: &B256| self.block(hash).map(|block| block.number()).ok_or_else(|| format!("block {hash} is not stored"));
            let (safe, finalized) = (number(&next.safe)?, number(&next.finalized)?);
            let mut w = self.db.write().map_err(|error| error.to_string())?;
            w.set_chain_state(finalized, safe).map_err(|error| error.to_string())?;
            Self::put_forkchoice(&mut w, next)?;
            w.commit().map_err(|error| error.to_string())?;
            self.forkchoice = next;
        }
        Ok(())
    }
}
