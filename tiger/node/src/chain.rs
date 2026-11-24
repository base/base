//! The node's block tree, canonical chain and state.
//!
//! State is a hash-addressed store of trie nodes and bytecodes: every block's state is its state
//! root, so the state of any stored block (canonical or side branch) can be read, executed on and
//! proven, and a reorg only moves the canonical pointer.
//!
//! Persistence is an append-only log under `<datadir>/tiger/chain.log`, written with
//! [`base_tiger_store::Fault`] hooks so crash-injection runs cover it. A block's nodes and codes
//! are written before the block, and every block and forkchoice record is synced before the
//! Engine API answers, so a crash loses at most what was not acknowledged. Reopening replays the
//! log and drops a torn tail. This log is the interim store until the base-reth-compatible store
//! (`tiger/store`) serves the same reads.

use std::{
    collections::HashMap,
    fs::{self, File, OpenOptions},
    io::Read,
    os::unix::fs::FileExt,
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
};

use alloy_consensus::Header;
use alloy_eips::eip2718::{Decodable2718, Encodable2718};
use alloy_primitives::{Address, B256, Bytes, keccak256};
use alloy_rlp::{RlpDecodable, RlpEncodable};
use base_common_consensus::{BaseReceiptEnvelope, BaseTxEnvelope as SignedTx};
use base_tiger_store::Fault;

use crate::{
    Executed, Genesis, NodeSource, WitnessState,
    genesis::genesis_codes,
    witness::build_state,
};

/// Blocks `BLOCKHASH` can reach.
const BLOCK_HASH_WINDOW: u64 = 256;

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
}

impl StoredBlock {
    fn new(
        header: Header,
        hash: B256,
        transactions: Vec<SignedTx>,
        senders: Vec<Address>,
        receipts: Vec<BaseReceiptEnvelope>,
    ) -> Self {
        assert_eq!(transactions.len(), senders.len());
        assert_eq!(transactions.len(), receipts.len());
        let tx_hashes = transactions.iter().map(|tx| tx.trie_hash()).collect();
        Self { header, hash, transactions, tx_hashes, senders, receipts }
    }

    /// The block number.
    pub const fn number(&self) -> u64 {
        self.header.number
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

/// Trie nodes and bytecodes by hash. Append-only: nothing is ever removed, so a state root that
/// was readable stays readable.
#[derive(Debug, Default)]
pub struct NodeStore {
    nodes: RwLock<HashMap<B256, Bytes>>,
    codes: RwLock<HashMap<B256, Bytes>>,
}

impl NodeStore {
    fn insert_nodes(&self, nodes: impl IntoIterator<Item = Bytes>) -> Vec<Bytes> {
        let mut map = self.nodes.write().expect("node store lock");
        let mut added = Vec::new();
        for node in nodes {
            if let std::collections::hash_map::Entry::Vacant(entry) = map.entry(keccak256(&node)) {
                entry.insert(node.clone());
                added.push(node);
            }
        }
        added
    }

    fn insert_codes(&self, codes: impl IntoIterator<Item = Bytes>) -> Vec<Bytes> {
        let mut map = self.codes.write().expect("code store lock");
        let mut added = Vec::new();
        for code in codes {
            if let std::collections::hash_map::Entry::Vacant(entry) = map.entry(keccak256(&code)) {
                entry.insert(code.clone());
                added.push(code);
            }
        }
        added
    }
}

impl NodeSource for NodeStore {
    fn trie_node(&self, hash: &B256) -> Option<Bytes> {
        self.nodes.read().expect("node store lock").get(hash).cloned()
    }

    fn code(&self, hash: &B256) -> Option<Bytes> {
        self.codes.read().expect("code store lock").get(hash).cloned()
    }
}

/// A stored block as logged.
#[derive(Debug, RlpEncodable, RlpDecodable)]
struct BlockRecord {
    header: Header,
    transactions: Vec<Bytes>,
    senders: Vec<Address>,
    receipts: Vec<Bytes>,
}

/// Log record kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum Kind {
    Genesis = 0,
    Node = 1,
    Code = 2,
    Block = 3,
    Forkchoice = 4,
}

impl Kind {
    const fn from_byte(byte: u8) -> Option<Self> {
        Some(match byte {
            0 => Self::Genesis,
            1 => Self::Node,
            2 => Self::Code,
            3 => Self::Block,
            4 => Self::Forkchoice,
            _ => return None,
        })
    }
}

/// The append-only chain log: records of `kind (1) | length (4, LE) | payload | check (8)`,
/// where `check` is the first 8 bytes of `keccak(kind | payload)`.
#[derive(Debug)]
struct ChainLog {
    path: PathBuf,
    file: File,
    len: u64,
    buffer: Vec<u8>,
    commits: u64,
}

impl ChainLog {
    /// Largest record accepted when reading (a block of 4 MB of transactions plus receipts).
    const MAX_RECORD: u32 = 256 << 20;
    const HEADER: usize = 5;
    const CHECK: usize = 8;

    fn check(kind: u8, payload: &[u8]) -> [u8; 8] {
        let mut data = Vec::with_capacity(payload.len() + 1);
        data.push(kind);
        data.extend_from_slice(payload);
        keccak256(&data)[..8].try_into().expect("8 bytes")
    }

    /// Opens the log, returning the valid records and truncating a torn tail.
    fn open(dir: &Path) -> Result<(Self, Vec<(Kind, Vec<u8>)>), String> {
        fs::create_dir_all(dir).map_err(|error| format!("creating {}: {error}", dir.display()))?;
        let path = dir.join("chain.log");
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|error| format!("opening {}: {error}", path.display()))?;
        let mut data = Vec::new();
        file.read_to_end(&mut data).map_err(|error| format!("reading {}: {error}", path.display()))?;
        let mut records = Vec::new();
        let mut at = 0usize;
        while data.len() - at >= Self::HEADER {
            let kind_byte = data[at];
            let length = u32::from_le_bytes(data[at + 1..at + 5].try_into().expect("4 bytes"));
            let Some(kind) = Kind::from_byte(kind_byte) else { break };
            if length > Self::MAX_RECORD {
                break;
            }
            let end = at + Self::HEADER + length as usize + Self::CHECK;
            if end > data.len() {
                break;
            }
            let payload = &data[at + Self::HEADER..end - Self::CHECK];
            if data[end - Self::CHECK..end] != Self::check(kind_byte, payload) {
                break;
            }
            records.push((kind, payload.to_vec()));
            at = end;
        }
        let len = at as u64;
        if len != data.len() as u64 {
            eprintln!(
                "tiger: dropping {} bytes of torn chain log tail at offset {len}",
                data.len() as u64 - len
            );
            file.set_len(len).map_err(|error| format!("truncating {}: {error}", path.display()))?;
            file.sync_all().map_err(|error| format!("syncing {}: {error}", path.display()))?;
        }
        Ok((Self { path, file, len, buffer: Vec::new(), commits: 0 }, records))
    }

    fn push(&mut self, kind: Kind, payload: &[u8]) {
        let length = u32::try_from(payload.len()).expect("records are below 4 GB");
        assert!(length <= Self::MAX_RECORD, "record of {length} bytes is too large");
        self.buffer.push(kind as u8);
        self.buffer.extend_from_slice(&length.to_le_bytes());
        self.buffer.extend_from_slice(payload);
        self.buffer.extend_from_slice(&Self::check(kind as u8, payload));
    }

    /// Writes the buffered records and syncs them.
    fn commit(&mut self, step: &str) -> Result<(), String> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        self.commits += 1;
        Fault::phase(self.commits, step);
        Fault::before_write(&self.path, self.len, &self.buffer);
        self.file
            .write_all_at(&self.buffer, self.len)
            .map_err(|error| format!("writing {}: {error}", self.path.display()))?;
        self.file.sync_data().map_err(|error| format!("syncing {}: {error}", self.path.display()))?;
        Fault::after_sync(&self.path);
        self.len += self.buffer.len() as u64;
        self.buffer.clear();
        Ok(())
    }
}

/// The block tree, the canonical chain, and the forkchoice heads.
#[derive(Debug)]
pub struct Chain {
    /// The chain and its genesis.
    pub genesis: Genesis,
    /// Trie nodes and codes of every stored state.
    pub store: Arc<NodeStore>,
    blocks: HashMap<B256, Arc<StoredBlock>>,
    /// Canonical block hashes, indexed by number minus the genesis number.
    canonical: Vec<B256>,
    /// Canonical transactions by hash: block number and index.
    tx_index: HashMap<B256, (u64, usize)>,
    forkchoice: Forkchoice,
    log: ChainLog,
}

impl Chain {
    /// Opens (or initializes) the chain under `datadir`.
    pub fn open(datadir: &Path, genesis: Genesis) -> Result<Self, String> {
        Fault::init();
        let (log, records) = ChainLog::open(&datadir.join("tiger"))?;
        let genesis_block = Arc::new(StoredBlock::new(
            genesis.header.clone(),
            genesis.hash,
            Vec::new(),
            Vec::new(),
            Vec::new(),
        ));
        let mut chain = Self {
            store: Arc::new(NodeStore::default()),
            blocks: HashMap::from([(genesis.hash, genesis_block)]),
            canonical: vec![genesis.hash],
            tx_index: HashMap::new(),
            forkchoice: Forkchoice { head: genesis.hash, safe: genesis.hash, finalized: genesis.hash },
            log,
            genesis,
        };
        if records.is_empty() {
            let alloc = chain.genesis.alloc.clone().ok_or(
                "a new datadir for a built-in chain needs a genesis file with its full allocation",
            )?;
            let state = build_state(&alloc);
            assert_eq!(state.root, chain.genesis.header.state_root);
            chain.log.push(Kind::Genesis, chain.genesis.hash.as_slice());
            for node in chain.store.insert_nodes(state.nodes) {
                chain.log.push(Kind::Node, &node);
            }
            for code in chain.store.insert_codes(genesis_codes(&alloc)) {
                chain.log.push(Kind::Code, &code);
            }
            chain.log.commit("genesis")?;
            return Ok(chain);
        }
        let mut forkchoice = chain.forkchoice;
        for (index, (kind, payload)) in records.into_iter().enumerate() {
            match kind {
                Kind::Genesis => {
                    if index != 0 || payload != chain.genesis.hash.as_slice() {
                        return Err(format!(
                            "datadir holds another chain (genesis {})",
                            alloy_primitives::hex::encode(&payload)
                        ));
                    }
                }
                Kind::Node => {
                    chain.store.insert_nodes([payload.into()]);
                }
                Kind::Code => {
                    chain.store.insert_codes([payload.into()]);
                }
                Kind::Block => {
                    let block = Self::decode_block(&payload)?;
                    chain.blocks.insert(block.hash, Arc::new(block));
                }
                Kind::Forkchoice => {
                    forkchoice = alloy_rlp::decode_exact(&payload)
                        .map_err(|error| format!("forkchoice record: {error}"))?;
                }
            }
        }
        chain.set_canonical(forkchoice.head)?;
        chain.forkchoice = forkchoice;
        Ok(chain)
    }

    fn decode_block(payload: &[u8]) -> Result<StoredBlock, String> {
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
        Ok(StoredBlock::new(record.header, hash, transactions, record.senders, receipts))
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

    /// The parent state of a child of `parent` (or the state after `parent`), with the block
    /// hashes `BLOCKHASH` can read along `parent`'s chain.
    pub fn state_after(&self, parent: &StoredBlock) -> WitnessState {
        let genesis_number = self.genesis.header.number;
        let mut hashes = Vec::with_capacity(BLOCK_HASH_WINDOW as usize);
        let mut at = Some((parent.number(), parent.hash, parent.header.parent_hash));
        while let Some((number, hash, parent_hash)) = at {
            if hashes.len() as u64 >= BLOCK_HASH_WINDOW {
                break;
            }
            hashes.push((number, hash));
            at = self.blocks.get(&parent_hash).map(|block| (block.number(), block.hash, block.header.parent_hash));
        }
        // Below the genesis block every block hash is zero.
        let first = parent.number().saturating_sub(BLOCK_HASH_WINDOW);
        let before_genesis = (first..genesis_number.min(parent.number())).map(|number| (number, B256::ZERO));
        let source: Arc<dyn NodeSource + Send + Sync> = self.store.clone();
        WitnessState::new(parent.header.state_root, source, hashes.into_iter().chain(before_genesis), [])
    }

    /// Stores an executed block (valid or not yet canonical) and syncs it.
    pub fn insert(&mut self, hash: B256, executed: &Executed) -> Result<Arc<StoredBlock>, String> {
        if let Some(block) = self.blocks.get(&hash) {
            return Ok(Arc::clone(block));
        }
        assert!(self.blocks.contains_key(&executed.header.parent_hash), "parent is stored");
        for node in self.store.insert_nodes(executed.update.nodes.iter().cloned()) {
            self.log.push(Kind::Node, &node);
        }
        let codes = executed.changes.code().map(|(_, code)| code.original_bytes());
        for code in self.store.insert_codes(codes.filter(|code| !code.is_empty())) {
            self.log.push(Kind::Code, &code);
        }
        let record = BlockRecord {
            header: executed.header.clone(),
            transactions: executed.transactions.iter().map(|tx| tx.encoded_2718().into()).collect(),
            senders: executed.senders.clone(),
            receipts: executed.receipts.iter().map(|receipt| receipt.encoded_2718().into()).collect(),
        };
        self.log.push(Kind::Block, &alloy_rlp::encode(&record));
        self.log.commit("block")?;
        let block = Arc::new(StoredBlock::new(
            executed.header.clone(),
            hash,
            executed.transactions.clone(),
            executed.senders.clone(),
            executed.receipts.clone(),
        ));
        self.blocks.insert(hash, Arc::clone(&block));
        Ok(block)
    }

    /// Makes `head` (a stored block) the canonical head, reorging as needed. The forkchoice
    /// heads are not persisted until [`Self::update_forkchoice`].
    pub fn set_canonical(&mut self, head: B256) -> Result<(), String> {
        let genesis_number = self.genesis.header.number;
        let mut branch = Vec::new();
        let mut at = head;
        loop {
            let block = self.blocks.get(&at).ok_or_else(|| format!("block {at} is not stored"))?;
            let index = (block.number() - genesis_number) as usize;
            if self.canonical.get(index) == Some(&at) {
                break;
            }
            if block.number() == genesis_number {
                return Err(format!("block {head} does not descend from the genesis block"));
            }
            branch.push(Arc::clone(block));
            at = block.header.parent_hash;
        }
        let fork_number = self.blocks[&at].number();
        let keep = (fork_number - genesis_number) as usize + 1;
        for hash in self.canonical.drain(keep..) {
            for tx in &self.blocks[&hash].tx_hashes {
                self.tx_index.remove(tx);
            }
        }
        for block in branch.into_iter().rev() {
            for (index, tx) in block.tx_hashes.iter().enumerate() {
                self.tx_index.insert(*tx, (block.number(), index));
            }
            self.canonical.push(block.hash);
        }
        assert_eq!(self.canonical.last(), Some(&head));
        Ok(())
    }

    /// Applies a forkchoice update whose blocks are all stored, and syncs it. `safe` and
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
            self.forkchoice = next;
            self.log.push(Kind::Forkchoice, &alloy_rlp::encode(next));
            self.log.commit("forkchoice")?;
        }
        Ok(())
    }
}
