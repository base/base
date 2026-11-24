//! Parent state served from a stateless execution witness.
//!
//! Reads walk the account and storage tries from the parent state root, revealing witness nodes
//! on the way, so every value is verified against the root by hash. A read whose path leaves the
//! witness is a [`WitnessError`], never a default: the first one is kept and reported.

use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet, HashMap},
    rc::Rc,
    sync::Arc,
};

use alloy_primitives::{Address, B256, Bytes, KECCAK256_EMPTY, U256, keccak256};
use alloy_rlp::Decodable;
use alloy_trie::{EMPTY_ROOT_HASH, Nibbles, TrieAccount};
use base_proof_mpt::{TrieNode, TrieNodeError, TrieProvider};
use evm2::{
    AccountInfo, BlockStateAccumulator, bytecode::Bytecode, evm::Database,
};

/// Data a witness does not contain, or cannot provide.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WitnessError {
    /// A trie node on the path to a key is not in the witness.
    MissingTrieNode(B256),
    /// A bytecode the block loads is not in the witness.
    MissingCode(B256),
    /// `BLOCKHASH` asked for a block that is not among the ancestors.
    MissingBlockHash(u64),
    /// A trie node or leaf could not be decoded.
    Malformed(String),
}

impl core::fmt::Display for WitnessError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::MissingTrieNode(hash) => write!(f, "missing trie node {hash} in witness"),
            Self::MissingCode(hash) => write!(f, "missing bytecode {hash} in witness"),
            Self::MissingBlockHash(number) => write!(f, "missing ancestor header for block {number}"),
            Self::Malformed(error) => write!(f, "malformed witness trie: {error}"),
        }
    }
}

impl std::error::Error for WitnessError {}

/// Where trie nodes and bytecodes come from: a request's witness, or the node's own store.
///
/// Every value is addressed by its keccak hash, so a source can withhold data but not change it.
pub trait NodeSource: core::fmt::Debug {
    /// The RLP encoding of the trie node with hash `hash`.
    fn trie_node(&self, hash: &B256) -> Option<Bytes>;
    /// The bytecode with hash `hash`.
    fn code(&self, hash: &B256) -> Option<Bytes>;
}

/// The nodes and codes of one stateless witness, keyed by their hashes.
#[derive(Debug, Default)]
pub struct WitnessNodes {
    nodes: HashMap<B256, Bytes>,
    codes: HashMap<B256, Bytes>,
}

impl WitnessNodes {
    /// Indexes witness trie nodes and bytecodes by hash.
    pub fn new(nodes: &[Bytes], codes: &[Bytes]) -> Self {
        Self {
            nodes: nodes.iter().map(|node| (keccak256(node), node.clone())).collect(),
            codes: codes.iter().map(|code| (keccak256(code), code.clone())).collect(),
        }
    }
}

impl NodeSource for WitnessNodes {
    fn trie_node(&self, hash: &B256) -> Option<Bytes> {
        self.nodes.get(hash).cloned()
    }

    fn code(&self, hash: &B256) -> Option<Bytes> {
        self.codes.get(hash).cloned()
    }
}

/// Trie node preimages by hash, remembering the first node a read needed and did not find.
#[derive(Debug)]
struct Nodes {
    source: Arc<dyn NodeSource + Send + Sync>,
    missing: RefCell<Option<WitnessError>>,
}

impl Nodes {
    fn record(&self, error: WitnessError) -> WitnessError {
        self.missing.borrow_mut().get_or_insert(error).clone()
    }

    fn trie_error(&self, error: TrieNodeError) -> WitnessError {
        self.missing.borrow().clone().unwrap_or_else(|| WitnessError::Malformed(error.to_string()))
    }

    fn open_account(
        &self,
        trie: &mut TrieNode,
        hashed_address: B256,
    ) -> Result<Option<TrieAccount>, WitnessError> {
        let leaf = trie
            .open(&Nibbles::unpack(hashed_address), self)
            .map_err(|error| self.trie_error(error))?;
        leaf.map(|rlp| {
            TrieAccount::decode(&mut rlp.as_ref())
                .map_err(|error| WitnessError::Malformed(error.to_string()))
        })
        .transpose()
    }

    /// Applies slot writes (hashed slot, value; zero deletes) to `trie`.
    fn apply_storage(
        &self,
        trie: &mut TrieNode,
        writes: &BTreeMap<B256, U256>,
    ) -> Result<(), WitnessError> {
        for (slot, value) in writes {
            let path = Nibbles::unpack(slot);
            if value.is_zero() {
                match trie.delete(&path, self) {
                    Ok(()) | Err(TrieNodeError::KeyNotFound) => {}
                    Err(error) => return Err(self.trie_error(error)),
                }
            } else {
                trie.insert(&path, alloy_rlp::encode(value).into(), self)
                    .map_err(|error| self.trie_error(error))?;
            }
        }
        Ok(())
    }
}

/// Appends to `out` the encoding of every unblinded node of `node` that its parent references
/// by hash (encodings of 32 bytes or more), and of the root itself.
fn collect_nodes(node: &TrieNode, root: bool, out: &mut Vec<Bytes>) {
    match node {
        TrieNode::Empty | TrieNode::Blinded { .. } => return,
        TrieNode::Leaf { .. } => {}
        TrieNode::Extension { node: child, .. } => collect_nodes(child, false, out),
        TrieNode::Branch { stack } => {
            for child in stack {
                collect_nodes(child, false, out);
            }
        }
    }
    let encoded = alloy_rlp::encode(node);
    if root || encoded.len() >= 32 {
        out.push(encoded.into());
    }
}

/// The state after a block: its root, and the trie nodes it references that the parent state
/// may not hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateUpdate {
    /// The state root.
    pub root: B256,
    /// Encodings of the account and storage trie nodes on the paths the block read or wrote.
    pub nodes: Vec<Bytes>,
}

impl TrieProvider for Nodes {
    type Error = WitnessError;

    fn trie_node_by_hash(&self, key: B256) -> Result<TrieNode, WitnessError> {
        let Some(node) = self.source.trie_node(&key) else {
            return Err(self.record(WitnessError::MissingTrieNode(key)));
        };
        TrieNode::decode(&mut node.as_ref())
            .map_err(|error| WitnessError::Malformed(format!("node {key}: {error}")))
    }
}

#[derive(Debug)]
struct Tries {
    accounts: TrieNode,
    /// Parent storage tries by hashed address, opened lazily.
    storages: HashMap<B256, TrieNode>,
}

/// The parent state of one block, served from its witness.
#[derive(Debug)]
pub struct WitnessState {
    nodes: Nodes,
    block_hashes: HashMap<u64, B256>,
    tries: RefCell<Tries>,
}

impl WitnessState {
    /// Creates the parent state rooted at `state_root`, reading nodes and codes from `source`.
    /// `storage_roots` are parent storage roots known without reading the account trie (the
    /// Isthmus `L2ToL1MessagePasser` root in the parent header).
    pub fn new(
        state_root: B256,
        source: Arc<dyn NodeSource + Send + Sync>,
        block_hashes: impl IntoIterator<Item = (u64, B256)>,
        storage_roots: impl IntoIterator<Item = (Address, B256)>,
    ) -> Self {
        Self {
            nodes: Nodes { source, missing: RefCell::new(None) },
            block_hashes: block_hashes.into_iter().collect(),
            tries: RefCell::new(Tries {
                accounts: TrieNode::new_blinded(state_root),
                storages: storage_roots
                    .into_iter()
                    .map(|(address, root)| (keccak256(address), TrieNode::new_blinded(root)))
                    .collect(),
            }),
        }
    }

    /// The first read the witness could not serve.
    pub fn missing(&self) -> Option<WitnessError> {
        self.nodes.missing.borrow().clone()
    }

    /// The parent account at `address`.
    pub fn account(&self, address: &Address) -> Result<Option<TrieAccount>, WitnessError> {
        let mut tries = self.tries.borrow_mut();
        self.nodes.open_account(&mut tries.accounts, keccak256(address))
    }

    /// The parent value of storage `slot` of `address`.
    pub fn storage(&self, address: &Address, slot: &U256) -> Result<U256, WitnessError> {
        let hashed_address = keccak256(address);
        let mut guard = self.tries.borrow_mut();
        let tries = &mut *guard;
        if !tries.storages.contains_key(&hashed_address) {
            let Some(account) = self.nodes.open_account(&mut tries.accounts, hashed_address)? else {
                return Ok(U256::ZERO);
            };
            tries.storages.insert(hashed_address, TrieNode::new_blinded(account.storage_root));
        }
        let trie = tries.storages.get_mut(&hashed_address).expect("opened above");
        let leaf = trie
            .open(&Nibbles::unpack(keccak256(slot.to_be_bytes::<32>())), &self.nodes)
            .map_err(|error| self.nodes.trie_error(error))?;
        leaf.map_or(Ok(U256::ZERO), |rlp| {
            U256::decode(&mut rlp.as_ref()).map_err(|error| WitnessError::Malformed(error.to_string()))
        })
    }

    /// The bytecode with hash `code_hash`.
    pub fn code(&self, code_hash: &B256) -> Result<Bytes, WitnessError> {
        if *code_hash == KECCAK256_EMPTY {
            return Ok(Bytes::new());
        }
        self.nodes
            .source
            .code(code_hash)
            .ok_or_else(|| self.nodes.record(WitnessError::MissingCode(*code_hash)))
    }

    /// The hash of ancestor block `number`.
    pub fn block_hash(&self, number: u64) -> Result<B256, WitnessError> {
        self.block_hashes
            .get(&number)
            .copied()
            .ok_or_else(|| self.nodes.record(WitnessError::MissingBlockHash(number)))
    }

    /// The parent storage trie of `hashed_address` (empty when the account does not exist).
    fn parent_storage_trie(&self, hashed_address: B256) -> Result<TrieNode, WitnessError> {
        let mut guard = self.tries.borrow_mut();
        let tries = &mut *guard;
        if let Some(trie) = tries.storages.get(&hashed_address) {
            return Ok(trie.clone());
        }
        let root = self
            .nodes
            .open_account(&mut tries.accounts, hashed_address)?
            .map_or(EMPTY_ROOT_HASH, |account| account.storage_root);
        Ok(TrieNode::new_blinded(root))
    }

    /// Storage root of `address` after `changes`, from its parent storage trie (or an empty one
    /// when the block wiped it).
    pub fn storage_root_after(
        &self,
        address: Address,
        changes: &BlockStateAccumulator,
    ) -> Result<B256, WitnessError> {
        let hashed_address = keccak256(address);
        let wiped = changes.storage_wipes().any(|wiped| wiped == address);
        let mut trie = if wiped {
            TrieNode::new_blinded(EMPTY_ROOT_HASH)
        } else {
            self.parent_storage_trie(hashed_address)?
        };
        let writes = changes
            .storage()
            .filter(|(key, _)| key.address() == address)
            .map(|(key, slot)| (keccak256(key.key().to_be_bytes::<32>()), slot.current))
            .collect();
        self.nodes.apply_storage(&mut trie, &writes)?;
        Ok(trie.blind())
    }

    /// The state after `changes`, applied to the parent state.
    pub fn state_root_after(
        &self,
        changes: &BlockStateAccumulator,
    ) -> Result<StateUpdate, WitnessError> {
        let mut writes: BTreeMap<B256, BTreeMap<B256, U256>> = BTreeMap::new();
        for (key, slot) in changes.storage() {
            writes
                .entry(keccak256(key.address()))
                .or_default()
                .insert(keccak256(key.key().to_be_bytes::<32>()), slot.current);
        }
        let wiped: BTreeSet<B256> = changes.storage_wipes().map(keccak256).collect();
        let infos: BTreeMap<B256, Option<AccountInfo>> = changes
            .accounts()
            .map(|(address, account)| (keccak256(address), account.current.clone()))
            .collect();
        let touched: BTreeSet<B256> =
            infos.keys().chain(writes.keys()).chain(wiped.iter()).copied().collect();

        let mut accounts = self.tries.borrow().accounts.clone();
        let nodes = &self.nodes;
        let mut new_nodes = Vec::new();
        for hashed_address in touched {
            let path = Nibbles::unpack(hashed_address);
            let parent = nodes.open_account(&mut accounts, hashed_address)?;
            let info = match infos.get(&hashed_address) {
                Some(info) => info.as_ref().map(|info| (info.nonce, info.balance, info.code_hash)),
                None => parent.map(|account| (account.nonce, account.balance, account.code_hash)),
            };
            let Some((nonce, balance, code_hash)) = info else {
                match accounts.delete(&path, nodes) {
                    Ok(()) | Err(TrieNodeError::KeyNotFound) => {}
                    Err(error) => return Err(nodes.trie_error(error)),
                }
                continue;
            };
            let empty = BTreeMap::new();
            let slot_writes = writes.get(&hashed_address).unwrap_or(&empty);
            let storage_root = if !wiped.contains(&hashed_address) && slot_writes.is_empty() {
                parent.map_or(EMPTY_ROOT_HASH, |account| account.storage_root)
            } else {
                let mut trie = if wiped.contains(&hashed_address) {
                    TrieNode::new_blinded(EMPTY_ROOT_HASH)
                } else {
                    self.parent_storage_trie(hashed_address)?
                };
                nodes.apply_storage(&mut trie, slot_writes)?;
                collect_nodes(&trie, true, &mut new_nodes);
                trie.blind()
            };
            let leaf = alloy_rlp::encode(TrieAccount { nonce, balance, storage_root, code_hash });
            accounts.insert(&path, leaf.into(), nodes).map_err(|error| nodes.trie_error(error))?;
        }
        collect_nodes(&accounts, true, &mut new_nodes);
        Ok(StateUpdate { root: accounts.blind(), nodes: new_nodes })
    }
}

/// The evm2 database over a shared [`WitnessState`].
#[derive(Debug, Clone)]
pub struct WitnessDb(pub Rc<WitnessState>);

impl Database for WitnessDb {
    type Error = WitnessError;

    fn get_account(&mut self, address: &Address) -> Result<Option<AccountInfo>, WitnessError> {
        Ok(self.0.account(address)?.map(|account| AccountInfo {
            balance: account.balance,
            nonce: account.nonce,
            code_hash: account.code_hash,
            code: None,
            ..AccountInfo::default()
        }))
    }

    fn get_code_by_hash(&mut self, code_hash: &B256) -> Result<Bytecode, WitnessError> {
        Ok(Bytecode::new_raw(self.0.code(code_hash)?))
    }

    fn get_storage(&mut self, address: &Address, key: &U256) -> Result<U256, WitnessError> {
        self.0.storage(address, key)
    }

    fn get_block_hash(&mut self, number: &U256) -> Result<B256, WitnessError> {
        self.0.block_hash(number.saturating_to())
    }
}
