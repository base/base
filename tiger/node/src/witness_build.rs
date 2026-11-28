//! Builds a block's execution witness on a full node for tp2p peers (feature `tp2p`), in the
//! shape of base-reth's `debug_executionWitness` without key preimages (state nodes, codes,
//! headers), from the keys the block's
//! validation read ([`crate::Reads`], recorded by [`crate::NodeState::recording`]). No
//! execution: only proofs at the parent state.
//!
//! The witness holds:
//! - the parent-state proof of every account and slot the block read (writes are reads: the EVM
//!   loads an account or slot before it changes it);
//! - for each key the block deleted (an account removed, a slot set to zero), the proofs along
//!   the siblings a branch on its path may collapse into, since the post-state root needs those
//!   nodes and no read loads them;
//! - the code of every code hash read;
//! - the headers from the oldest block `BLOCKHASH` read up to the parent.
//!
//! A stateless node checks every node by hash against its own parent state root, so a witness
//! from a peer can be incomplete but never wrong.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
    sync::{Arc, Condvar, Mutex, mpsc},
    time::{Duration, Instant},
};

use alloy_primitives::{Address, B256, Bytes, KECCAK256_EMPTY, U256, keccak256};
use alloy_rlp::Decodable;
use alloy_trie::{Nibbles, nodes::TrieNode};

use crate::{Chain, ExecutionWitness, Node, StoredBlock};

/// Every account, storage slot, code hash and ancestor block hash a block's execution read: what
/// its execution witness must prove ([`crate::NodeState::recording`]).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Reads {
    /// Accounts.
    pub accounts: BTreeSet<Address>,
    /// Storage slots by account.
    pub storage: BTreeMap<Address, BTreeSet<U256>>,
    /// Code hashes.
    pub codes: BTreeSet<B256>,
    /// Ancestor block numbers read by `BLOCKHASH`.
    pub block_numbers: BTreeSet<u64>,
}

/// Witnesses of the most recent blocks a full node imported, built in the background from the
/// keys their validation recorded, so peers (tp2p) or an RPC can be served without executing a
/// block again. Off unless the node is started with it.
#[derive(Debug)]
pub struct WitnessCache {
    reads: Mutex<HashMap<B256, Reads>>,
    built: Mutex<Built>,
    ready: Condvar,
    jobs: Mutex<Option<mpsc::SyncSender<B256>>>,
    capacity: usize,
}

/// Built witnesses (RLP-encoded, see [`encode_witness`]) by block hash, oldest first.
#[derive(Debug, Default)]
struct Built {
    witnesses: HashMap<B256, (u64, Arc<Vec<u8>>)>,
    order: VecDeque<B256>,
    listeners: Vec<mpsc::Sender<(B256, u64)>>,
}

/// Recorded reads kept for blocks not built yet; more are dropped.
const MAX_PENDING_READS: usize = 256;

impl WitnessCache {
    /// A cache of the last `capacity` witnesses.
    pub fn new(capacity: usize) -> Self {
        Self {
            reads: Mutex::new(HashMap::new()),
            built: Mutex::new(Built::default()),
            ready: Condvar::new(),
            jobs: Mutex::new(None),
            capacity,
        }
    }

    /// Keeps the reads block `hash`'s validation recorded, for its witness once it is stored.
    pub fn put_reads(&self, hash: B256, reads: Reads) {
        let mut pending = self.reads.lock().expect("witness reads");
        if pending.len() >= MAX_PENDING_READS {
            pending.clear();
        }
        pending.insert(hash, reads);
    }

    /// Queues block `hash` (stored valid) for its witness. Dropped when the builder is behind.
    pub fn stored(&self, hash: B256) {
        if let Some(jobs) = &*self.jobs.lock().expect("witness jobs") {
            let _ = jobs.try_send(hash);
        }
    }

    /// The RLP-encoded witness of block `hash`, waiting up to `wait` for it to be built.
    pub fn get(&self, hash: &B256, wait: Duration) -> Option<Arc<Vec<u8>>> {
        let built = self.built.lock().expect("witness cache");
        let (built, _) = self
            .ready
            .wait_timeout_while(built, wait, |built| !built.witnesses.contains_key(hash))
            .expect("witness cache");
        built.witnesses.get(hash).map(|(_, witness)| Arc::clone(witness))
    }

    /// A channel that receives `(hash, number)` of each witness once built.
    pub fn subscribe(&self) -> mpsc::Receiver<(B256, u64)> {
        let (sender, receiver) = mpsc::channel();
        self.built.lock().expect("witness cache").listeners.push(sender);
        receiver
    }

    fn insert(&self, hash: B256, number: u64, witness: Vec<u8>) {
        let mut built = self.built.lock().expect("witness cache");
        built.witnesses.insert(hash, (number, Arc::new(witness)));
        built.order.push_back(hash);
        while built.order.len() > self.capacity {
            let oldest = built.order.pop_front().expect("over capacity");
            built.witnesses.remove(&oldest);
        }
        built.listeners.retain(|listener| listener.send((hash, number)).is_ok());
        self.ready.notify_all();
    }
}

impl Node {
    /// Starts the thread that builds the witness of each block stored valid (no-op without a
    /// witness cache).
    pub fn start_witness_builder(self: &Arc<Self>) -> Result<(), String> {
        let Some(cache) = &self.witness_cache else { return Ok(()) };
        let (sender, receiver) = mpsc::sync_channel::<B256>(64);
        *cache.jobs.lock().expect("witness jobs") = Some(sender);
        let node = Arc::clone(self);
        std::thread::Builder::new()
            .name("witness-build".into())
            .spawn(move || {
                let cache = node.witness_cache.as_ref().expect("checked above");
                while let Ok(hash) = receiver.recv() {
                    let Some(reads) = cache.reads.lock().expect("witness reads").remove(&hash) else { continue };
                    let started = Instant::now();
                    let chain = node.chain.read().expect("chain lock");
                    let Some(block) = chain.block(&hash) else { continue };
                    let built = build_witness(&chain, &block, &reads);
                    drop(chain);
                    match built {
                        Ok(witness) => {
                            let encoded = encode_witness(&witness);
                            if std::env::var_os("TIGER_VALIDATION_LOG").is_some() {
                                crate::Log::line(format!(
                                    "tiger: built witness of block {} in {:.1}ms: {} nodes, {} codes, {} bytes",
                                    block.number(),
                                    started.elapsed().as_secs_f64() * 1e3,
                                    witness.state.len(),
                                    witness.codes.len(),
                                    encoded.len()
                                ));
                            }
                            cache.insert(hash, block.number(), encoded);
                        }
                        Err(error) => eprintln!("tiger: building the witness of block {} {hash}: {error}", block.number()),
                    }
                }
            })
            .map_err(|error| error.to_string())?;
        Ok(())
    }
}

/// A witness as RLP: `[state, codes, headers]`, each a list of byte strings.
pub fn encode_witness(witness: &ExecutionWitness) -> Vec<u8> {
    use alloy_rlp::Encodable;
    let lists = [&witness.state, &witness.codes, &witness.headers];
    let payload: usize = lists.iter().map(|list| list.length()).sum();
    let mut out = Vec::with_capacity(payload + 9);
    alloy_rlp::Header { list: true, payload_length: payload }.encode(&mut out);
    for list in lists {
        list.encode(&mut out);
    }
    out
}

/// Decodes [`encode_witness`]'s format, refusing trailing bytes.
pub fn decode_witness(mut bytes: &[u8]) -> Result<ExecutionWitness, String> {
    let header = alloy_rlp::Header::decode(&mut bytes).map_err(|error| error.to_string())?;
    if !header.list || header.payload_length != bytes.len() {
        return Err("witness is not one RLP list".into());
    }
    let mut list = || Vec::<Bytes>::decode(&mut bytes).map_err(|error| error.to_string());
    let witness = ExecutionWitness { state: list()?, codes: list()?, headers: list()? };
    if !bytes.is_empty() {
        return Err("trailing bytes after the witness".into());
    }
    Ok(witness)
}

/// The witness of `block`, which `reads` were recorded for, built over its parent's state.
pub fn build_witness(chain: &Chain, block: &StoredBlock, reads: &Reads) -> Result<ExecutionWitness, String> {
    let parent = chain
        .block(&block.header.parent_hash)
        .ok_or_else(|| format!("parent of block {} {} is not held", block.number(), block.hash))?;
    let state = chain.state_after(&parent)?;

    let mut keys: BTreeMap<Address, BTreeSet<U256>> = reads.accounts.iter().map(|a| (*a, BTreeSet::new())).collect();
    for (address, slots) in &reads.storage {
        keys.entry(*address).or_default().extend(slots.iter().copied());
    }
    // Deleted keys: the block's in-memory changes (absent for a block read back from disk, whose
    // witness then may lack collapse siblings).
    let mut deleted_accounts = Vec::new();
    let mut deleted_slots: BTreeMap<Address, Vec<U256>> = BTreeMap::new();
    if let Some(changes) = &block.changes {
        for (address, account) in &changes.accounts {
            keys.entry(*address).or_default();
            if account.is_none() {
                deleted_accounts.push(*address);
            }
        }
        for (address, slot, value) in &changes.storage {
            keys.entry(*address).or_default().insert(*slot);
            if value.is_zero() {
                deleted_slots.entry(*address).or_default().push(*slot);
            }
        }
    }

    let mut nodes = NodeSet::default();
    let mut account_proofs: BTreeMap<Address, Vec<Bytes>> = BTreeMap::new();
    let mut storage_proofs: BTreeMap<(Address, U256), Vec<Bytes>> = BTreeMap::new();
    for (address, slots) in &keys {
        let slots: Vec<U256> = slots.iter().copied().collect();
        let proof = state.proof(*address, &slots)?;
        nodes.extend(&proof.proof);
        for (slot, storage) in slots.iter().zip(&proof.storage) {
            nodes.extend(&storage.proof);
            storage_proofs.insert((*address, *slot), storage.proof.clone());
        }
        account_proofs.insert(*address, proof.proof);
    }

    // Account trie siblings of deleted accounts.
    let targets: Vec<(Nibbles, &[Bytes])> = deleted_accounts
        .iter()
        .map(|address| (Nibbles::unpack(keccak256(address)), account_proofs[address].as_slice()))
        .collect();
    for path in collapse_siblings(&targets) {
        nodes.extend(&state.proof_hashed(path_key(&path), &[])?.proof);
    }
    // Storage trie siblings of zeroed slots, per account.
    for (address, slots) in &deleted_slots {
        let targets: Vec<(Nibbles, &[Bytes])> = slots
            .iter()
            .map(|slot| (Nibbles::unpack(keccak256(B256::from(*slot))), storage_proofs[&(*address, *slot)].as_slice()))
            .collect();
        let siblings: Vec<B256> = collapse_siblings(&targets).iter().map(path_key).collect();
        if siblings.is_empty() {
            continue;
        }
        for storage in state.proof_hashed(keccak256(address), &siblings)?.storage {
            nodes.extend(&storage.proof);
        }
    }

    let mut codes = Vec::with_capacity(reads.codes.len());
    for hash in &reads.codes {
        if *hash != KECCAK256_EMPTY && *hash != B256::ZERO {
            codes.push(state.code(hash)?);
        }
    }
    drop(state);

    // Headers from the oldest block hash read up to the parent, as base-reth includes them.
    let hashes = chain.block_hashes(&parent);
    let oldest = reads.block_numbers.first().copied().unwrap_or(parent.number()).min(parent.number());
    let mut headers = Vec::new();
    for number in oldest..=parent.number() {
        let Some(hash) = hashes.get(&number) else { continue };
        if let Some(ancestor) = chain.block(hash) {
            headers.push(Bytes::from(alloy_rlp::encode(&ancestor.header)));
        }
    }

    Ok(ExecutionWitness { state: nodes.into_vec(), codes, headers })
}

/// Trie nodes, each once, in first-seen order.
#[derive(Default)]
struct NodeSet {
    seen: std::collections::HashSet<B256>,
    nodes: Vec<Bytes>,
}

impl NodeSet {
    fn extend(&mut self, nodes: &[Bytes]) {
        for node in nodes {
            if self.seen.insert(keccak256(node)) {
                self.nodes.push(node.clone());
            }
        }
    }

    fn into_vec(self) -> Vec<Bytes> {
        self.nodes
    }
}

/// The hashed key whose path starts with `path` and continues with zeros.
fn path_key(path: &Nibbles) -> B256 {
    let mut nibbles = [0u8; 64];
    for (i, nibble) in path.to_vec().into_iter().enumerate().take(64) {
        nibbles[i] = nibble;
    }
    let mut key = [0u8; 32];
    for (i, byte) in key.iter_mut().enumerate() {
        *byte = (nibbles[2 * i] << 4) | nibbles[2 * i + 1];
    }
    B256::from(key)
}

/// The paths of the children that deleting `targets` may collapse a branch into. Each target is
/// a hashed key's path with its parent-state proof (root first). For every branch on those paths,
/// if at most one of its children is left once the children the targets pass through are
/// discounted, every other child's path is returned: the post-state root merges the branch into
/// that child, so the witness needs the child's node.
fn collapse_siblings(targets: &[(Nibbles, &[Bytes])]) -> Vec<Nibbles> {
    // Branch path -> (children present, children a target passes through).
    let mut branches: BTreeMap<Vec<u8>, (u16, u16)> = BTreeMap::new();
    for (target, proof) in targets {
        let mut depth = 0;
        for rlp in *proof {
            let Ok(node) = TrieNode::decode(&mut &rlp[..]) else { break };
            match node {
                TrieNode::Branch(branch) if depth < target.len() => {
                    let entry = branches.entry(target.slice(..depth).to_vec()).or_insert((branch.state_mask.get(), 0));
                    entry.1 |= 1 << target.get_unchecked(depth);
                    depth += 1;
                }
                TrieNode::Extension(extension) if target.slice(depth..).starts_with(&extension.key) => {
                    depth += extension.key.len();
                }
                _ => break,
            }
        }
    }
    let mut out = Vec::new();
    for (path, (present, deleted)) in branches {
        let left = present & !deleted;
        if left.count_ones() > 1 {
            continue;
        }
        for nibble in 0..16u8 {
            if left & (1 << nibble) != 0 {
                let mut child = path.clone();
                child.push(nibble);
                out.push(Nibbles::from_nibbles(child));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use alloy_consensus::Header;
    use alloy_eips::eip2718::Encodable2718;
    use alloy_primitives::{Address, B256, Bytes, TxKind, U256};
    use base_common_consensus::{BaseTxEnvelope, TxDeposit};
    use serde_json::json;

    use super::build_witness;
    use crate::{BlockExecution, ExecutionWitness, Node, StoredBlock, rpc::test_utils::node_with};

    /// Clears storage slot 1 of itself.
    const CLEARER: &str = "0x00000000000000000000000000000000000000c0";
    /// An empty account, deleted when a call touches it (EIP-161).
    const EMPTY: &str = "0x00000000000000000000000000000000000000ee";

    fn call(to: Address, nonce: u8) -> Bytes {
        let tx = TxDeposit {
            source_hash: B256::with_last_byte(nonce),
            from: Address::with_last_byte(0x10),
            to: TxKind::Call(to),
            mint: 0,
            value: U256::ZERO,
            gas_limit: 200_000,
            is_system_transaction: false,
            input: Bytes::new(),
        };
        BaseTxEnvelope::Deposit(alloy_consensus::Sealed::new(tx)).encoded_2718().into()
    }

    /// Builds and stores a child of the genesis block with `transactions`, then validates it
    /// again over a recording state, returning the stored block and the reads.
    fn block_with_reads(node: &Node, transactions: &[Bytes]) -> (Arc<StoredBlock>, crate::Reads) {
        let mut chain = node.chain.write().unwrap();
        let parent = chain.head();
        let header = Header {
            parent_hash: parent.hash,
            number: parent.number() + 1,
            timestamp: parent.header.timestamp + 2,
            gas_limit: parent.header.gas_limit,
            base_fee_per_gas: parent.header.base_fee_per_gas,
            beneficiary: parent.header.beneficiary,
            parent_beacon_block_root: Some(B256::ZERO),
            ..Header::default()
        };
        let state = chain.state_after(&parent).unwrap();
        let built = BlockExecution { chain: &node.spec }
            .build(&BlockExecution::env(&header), jovian_extra_data(), Some(0), transactions, false, &state)
            .unwrap();
        drop(state);
        let executed = built.executed.expect("the block builds");
        let hash = executed.header.hash_slow();
        let header = executed.header.clone();
        chain.insert(hash, &executed, None).unwrap();
        let state = chain.state_after(&parent).unwrap().recording();
        let validated = BlockExecution { chain: &node.spec }.validate(&header, transactions, &parent.header, &state).unwrap();
        assert!(validated.invalid.is_none(), "{:?}", validated.invalid);
        let reads = state.take_reads();
        drop(state);
        (chain.block(&hash).unwrap(), reads)
    }

    /// Jovian extra data: version 1, denominator 50, elasticity 6, minimum base fee 0.
    fn jovian_extra_data() -> Bytes {
        let mut extra = vec![1, 0, 0, 0, 50, 0, 0, 0, 6];
        extra.extend_from_slice(&[0; 8]);
        extra.into()
    }

    fn verify(node: &Node, block: &StoredBlock, witness: ExecutionWitness, transactions: &[Bytes]) -> Result<(), String> {
        let chain = node.chain.read().unwrap();
        let parent = chain.block(&block.header.parent_hash).unwrap();
        node.validate_with_witness(&chain, witness, &block.header, block.hash, transactions, &parent)
            .map(|(validated, _)| assert!(validated.invalid.is_none()))
            .map_err(|error| error.message)
    }

    /// A witness built from recorded reads lets the stateless path validate a block that clears
    /// one of two storage slots (the storage root collapses into the other slot's leaf, which no
    /// read loads) and deletes an empty account; without the collapse siblings it does not.
    #[test]
    fn built_witness_validates_a_block_with_deletions() {
        let node = node_with(
            0,
            json!({ "extraData": "0x0100000032000000060000000000000000", "alloc": {
                CLEARER: { "balance": "0x0", "code": "0x6000600155", "storage": {
                    "0x0000000000000000000000000000000000000000000000000000000000000001": "0x01",
                    "0x0000000000000000000000000000000000000000000000000000000000000002": "0x02"
                } },
                EMPTY: { "balance": "0x0" }
            } }),
        );
        let transactions = [call(CLEARER.parse().unwrap(), 1), call(EMPTY.parse().unwrap(), 2)];
        let (block, reads) = block_with_reads(&node, &transactions);
        let changes = block.changes.as_ref().expect("in-memory block keeps its changes");
        assert!(changes.storage.iter().any(|(_, slot, value)| *slot == U256::from(1) && value.is_zero()));
        assert!(!reads.storage[&CLEARER.parse::<Address>().unwrap()].contains(&U256::from(2)), "slot 2 is never read");

        let witness = {
            let chain = node.chain.read().unwrap();
            build_witness(&chain, &block, &reads).unwrap()
        };
        assert!(!witness.headers.is_empty(), "the parent header is included");
        let encoded = super::encode_witness(&witness);
        assert_eq!(super::encode_witness(&super::decode_witness(&encoded).unwrap()), encoded);
        verify(&node, &block, witness, &transactions).expect("the built witness verifies");

        // Without the deletion info (no collapse siblings) the witness is incomplete.
        let mut bare = (*block).clone();
        bare.changes = None;
        let bare_witness = {
            let chain = node.chain.read().unwrap();
            build_witness(&chain, &bare, &reads).unwrap()
        };
        let error = verify(&node, &block, bare_witness, &transactions).expect_err("siblings are needed");
        assert!(error.contains("incomplete"), "{error}");
    }
}
