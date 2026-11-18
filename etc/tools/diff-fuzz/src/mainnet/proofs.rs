//! Execution witnesses rebuilt from `eth_getProof`, for endpoints that serve proofs and struct
//! log traces but not `debug_executionWitness`.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    sync::Mutex,
    thread,
    time::Duration,
};

use alloy_consensus::{Transaction, transaction::SignerRecoverable};
use alloy_eips::Decodable2718;
use alloy_primitives::{Address, B256, Bytes, KECCAK256_EMPTY, TxKind, U256, address, keccak256};
use alloy_rlp::Decodable;
use alloy_rpc_types_eth::EIP1186AccountProofResponse;
use alloy_trie::{
    Nibbles,
    nodes::{BranchNode, TrieNode},
};
use base_common_consensus::{BaseTxEnvelope, Predeploys, SystemAddresses};
use eyre::{bail, eyre};
use serde::Deserialize;
use serde_json::{Value, json};

use super::RpcClient;

/// One step of a `debug_traceBlockByNumber` struct log, with only the fields the witness needs.
#[derive(Debug, Deserialize)]
struct Step {
    op: String,
    depth: usize,
    #[serde(default)]
    stack: Vec<U256>,
}

#[derive(Debug, Deserialize)]
struct TxTrace {
    #[serde(rename = "structLogs", default)]
    struct_logs: Vec<Step>,
}

#[derive(Debug, Deserialize)]
struct TraceEntry {
    result: TxTrace,
}

/// State a block reads, as accounts with their storage slots, plus the block numbers its
/// `BLOCKHASH` reads ask for.
#[derive(Debug, Default)]
pub struct Touched {
    /// Accounts and the slots read or written in each.
    pub accounts: BTreeMap<Address, BTreeSet<B256>>,
    /// Block numbers passed to `BLOCKHASH`.
    pub block_hashes: BTreeSet<u64>,
    /// Slots some `SSTORE` sets to zero; clearing one can collapse a branch of the storage trie.
    pub cleared: BTreeSet<(Address, B256)>,
}

impl Touched {
    fn account(&mut self, address: Address) -> &mut BTreeSet<B256> {
        self.accounts.entry(address).or_default()
    }
}

/// Builds the witness of a block from struct log traces and `eth_getProof` at its parent.
///
/// The touched set comes from the transactions themselves (senders, recipients, deployment
/// addresses, access lists, EIP-7702 authorities), from every struct log step that names an
/// account or a slot (calls, balance and code reads, self-destructs, `SLOAD`/`SSTORE` with the
/// storage address of the running frame, created addresses, `BLOCKHASH`), and from the state the
/// block's system logic reads outside the EVM: fee vaults, the L1 block info slots the L1 cost
/// uses, the EIP-4788 and EIP-2935 ring buffer slots, and the `BaseTime` proxy slots. Each
/// account's proof (with its slots) and code are then fetched at the parent block.
///
/// Trie nodes that only a deletion needs (the sibling a collapsing branch keeps) are not on any
/// proof path, so a block that clears a slot or an account can come out incomplete; the
/// fetcher's validation and the adapters report it.
#[derive(Debug, Clone)]
pub struct ProofWitness {
    /// Endpoints serving `debug_traceBlockByNumber`.
    pub trace: RpcClient,
    /// Endpoints serving `eth_getProof` and `eth_getCode` at the parent block.
    pub rpc: RpcClient,
    /// Parallel proof requests.
    pub concurrency: usize,
}

impl ProofWitness {
    /// Attempts per proof or code request; load-balanced backends often lack recent state.
    pub const ATTEMPTS: u32 = 60;
    /// `L1Block` slots the L1 cost and operator fee read.
    pub const L1_BLOCK_SLOTS: u64 = 16;
    /// EIP-1967 implementation slot.
    pub const IMPLEMENTATION_SLOT: B256 = alloy_primitives::b256!(
        "0x360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc"
    );
    /// EIP-1967 admin slot.
    pub const ADMIN_SLOT: B256 = alloy_primitives::b256!(
        "0xb53127684a568b3173ae13b9f8a6016e243e63b6e8ee1178d6a717850b5d6103"
    );
    const BEACON_ROOTS: Address = address!("0x000F3df6D732807Ef1319fB7B8bB8522d0Beac02");
    const HISTORY: Address = address!("0x0000F90827F1C53a10cb7A02335B175320002935");
    const BASE_TIME: Address = address!("0x4200000000000000000000000000000000000030");
    const RING: u64 = 8191;
    /// Caller of the EIP-4788 and EIP-2935 system calls.
    const SYSTEM_CALLER: Address = address!("0xfffffffffffffffffffffffffffffffffffffffe");

    /// Collects the touched set of block `number` from its transactions and trace.
    pub fn touched(
        &self,
        number: u64,
        timestamp: u64,
        coinbase: Address,
        transactions: &[Bytes],
    ) -> eyre::Result<Touched> {
        let trace: Vec<TraceEntry> = serde_json::from_value(self.trace.call(
            "debug_traceBlockByNumber",
            json!([format!("{number:#x}"), {
                "disableMemory": true, "disableStorage": true, "enableReturnData": false
            }]),
        )?)?;
        if trace.len() != transactions.len() {
            bail!("trace has {} transactions, block has {}", trace.len(), transactions.len());
        }
        let mut touched = Touched::default();
        for address in [
            coinbase,
            Predeploys::SEQUENCER_FEE_VAULT,
            Predeploys::BASE_FEE_VAULT,
            Predeploys::L1_FEE_VAULT,
            Predeploys::OPERATOR_FEE_VAULT,
            Predeploys::L2_TO_L1_MESSAGE_PASSER,
            Self::SYSTEM_CALLER,
            SystemAddresses::DEPOSITOR_ACCOUNT,
        ] {
            touched.account(address);
        }
        let l1 = touched.account(Predeploys::L1_BLOCK_INFO);
        l1.extend((0..Self::L1_BLOCK_SLOTS).map(|slot| B256::from(U256::from(slot))));
        let beacon = timestamp % Self::RING;
        touched
            .account(Self::BEACON_ROOTS)
            .extend([B256::from(U256::from(beacon)), B256::from(U256::from(beacon + Self::RING))]);
        touched.account(Self::HISTORY).insert(B256::from(U256::from((number - 1) % Self::RING)));
        touched.account(Self::BASE_TIME).extend([Self::IMPLEMENTATION_SLOT, Self::ADMIN_SLOT]);

        for (raw, entry) in transactions.iter().zip(trace) {
            let tx = BaseTxEnvelope::decode_2718(&mut raw.as_ref())?;
            let from = match &tx {
                BaseTxEnvelope::Deposit(deposit) => deposit.from,
                other => other.recover_signer()?,
            };
            touched.account(from);
            let entry_address = match tx.kind() {
                TxKind::Call(to) => to,
                TxKind::Create => from.create(tx.nonce()),
            };
            touched.account(entry_address);
            if let Some(list) = tx.access_list() {
                for item in list.iter() {
                    touched.account(item.address).extend(item.storage_keys.iter().copied());
                }
            }
            if let Some(authorizations) = tx.authorization_list() {
                for authorization in authorizations {
                    if let Ok(authority) = authorization.recover_authority() {
                        touched.account(authority);
                    }
                    touched.account(authorization.address);
                }
            }
            Self::walk(&entry.result.struct_logs, entry_address, &mut touched);
        }
        Ok(touched)
    }

    /// Adds what one transaction's struct log reads.
    fn walk(steps: &[Step], entry: Address, touched: &mut Touched) {
        let to_address = |word: &U256| Address::from_word(B256::from(*word));
        // Storage address of the frame at each depth (index 0 unused; depth starts at 1).
        let mut frames: Vec<Option<Address>> = vec![None, Some(entry)];
        let mut pending_call: Option<(usize, Option<Address>)> = None;
        let mut pending_create: Option<usize> = None;
        for step in steps {
            if let Some((depth, address)) = pending_call.take()
                && step.depth == depth + 1
            {
                frames.truncate(depth + 1);
                frames.push(address);
            }
            if let Some(depth) = pending_create
                && step.depth == depth
            {
                pending_create = None;
                if let Some(created) = step.stack.last().map(to_address)
                    && created != Address::ZERO
                {
                    touched.account(created);
                }
            }
            let frame = frames.get(step.depth).copied().flatten();
            let stack = &step.stack;
            let top = stack.last();
            match step.op.as_str() {
                "SLOAD" | "SSTORE" => {
                    if let (Some(frame), Some(slot)) = (frame, top) {
                        touched.account(frame).insert(B256::from(*slot));
                        if step.op == "SSTORE"
                            && stack.len() >= 2
                            && stack[stack.len() - 2].is_zero()
                        {
                            touched.cleared.insert((frame, B256::from(*slot)));
                        }
                    }
                }
                "CALL" | "STATICCALL" | "CALLCODE" | "DELEGATECALL" => {
                    if let Some(target) = stack.len().checked_sub(2).map(|i| to_address(&stack[i]))
                    {
                        touched.account(target);
                        let storage = match step.op.as_str() {
                            "CALL" | "STATICCALL" => Some(target),
                            _ => frame,
                        };
                        pending_call = Some((step.depth, storage));
                    }
                }
                "CREATE" | "CREATE2" => {
                    // The new frame's address is only known when it returns; a fresh account
                    // has no storage to prove.
                    pending_call = Some((step.depth, None));
                    pending_create = Some(step.depth);
                }
                "BALANCE" | "EXTCODESIZE" | "EXTCODEHASH" | "EXTCODECOPY" | "SELFDESTRUCT" => {
                    if let Some(word) = top {
                        touched.account(to_address(word));
                    }
                }
                "BLOCKHASH" => {
                    if let Some(word) = top
                        && let Ok(number) = u64::try_from(*word)
                    {
                        touched.block_hashes.insert(number);
                    }
                }
                _ => {}
            }
        }
    }

    /// Fetches the proofs and codes of `touched` at block `parent`, returning trie nodes and
    /// bytecodes.
    pub fn fetch(&self, parent: u64, touched: &Touched) -> eyre::Result<(Vec<Bytes>, Vec<Bytes>)> {
        let mut queue: Vec<(Address, Vec<B256>)> = touched
            .accounts
            .iter()
            .map(|(address, slots)| (*address, slots.iter().copied().collect()))
            .collect();
        let nodes = Mutex::new((HashSet::<B256>::new(), Vec::<Bytes>::new()));
        let codes = Mutex::new(BTreeMap::<B256, Bytes>::new());
        let delegates = Mutex::new(Vec::<Address>::new());
        let storage_roots = Mutex::new(BTreeMap::<Address, B256>::new());
        let seen = Mutex::new(touched.accounts.keys().copied().collect::<HashSet<Address>>());
        let error = Mutex::new(None::<eyre::Report>);
        // EIP-7702 delegates loaded through a delegated account's code join a second round.
        for _round in 0..3 {
            if queue.is_empty() {
                break;
            }
            let work = Mutex::new(std::mem::take(&mut queue));
            thread::scope(|scope| {
                for _ in 0..self.concurrency.max(1) {
                    scope.spawn(|| {
                        loop {
                            let Some((address, slots)) = work.lock().unwrap().pop() else { break };
                            match self.account(parent, address, &slots) {
                                Ok((proof, code, storage_root)) => {
                                    storage_roots.lock().unwrap().insert(address, storage_root);
                                    let mut n = nodes.lock().unwrap();
                                    for node in proof {
                                        if n.0.insert(keccak256(&node)) {
                                            n.1.push(node);
                                        }
                                    }
                                    if let Some(code) = code {
                                        if code.len() == 23 && code.starts_with(&[0xef, 0x01, 0x00])
                                        {
                                            delegates
                                                .lock()
                                                .unwrap()
                                                .push(Address::from_slice(&code[3..]));
                                        }
                                        codes.lock().unwrap().insert(keccak256(&code), code);
                                    }
                                }
                                Err(e) => {
                                    error.lock().unwrap().get_or_insert(e);
                                }
                            }
                        }
                    });
                }
            });
            if let Some(e) = error.lock().unwrap().take() {
                return Err(e);
            }
            let mut seen = seen.lock().unwrap();
            for delegate in delegates.lock().unwrap().drain(..) {
                if seen.insert(delegate) {
                    queue.push((delegate, Vec::new()));
                }
            }
        }
        let (known, mut nodes) = nodes.into_inner().unwrap();
        // Clearing a slot whose branch keeps a single other child collapses that branch, which
        // needs the other child's node: prove a key that leads into it.
        let map: HashMap<B256, &Bytes> = nodes.iter().map(|n| (keccak256(n), n)).collect();
        let roots = storage_roots.into_inner().unwrap();
        let mut extra: Vec<(Address, B256)> = Vec::new();
        for (address, slot) in &touched.cleared {
            let Some(root) = roots.get(address) else { continue };
            if let Some(prefix) = Self::collapse_sibling(&map, *root, keccak256(slot))
                && prefix.len() <= Self::MAX_SEARCH_NIBBLES
                && let Some(key) = Self::key_with_prefix(&prefix)
            {
                extra.push((*address, key));
            }
        }
        let mut known = known;
        for (address, key) in extra {
            if let Ok((proof, _, _)) = self.account(parent, address, &[key]) {
                for node in proof {
                    if known.insert(keccak256(&node)) {
                        nodes.push(node);
                    }
                }
            }
        }
        Ok((nodes, codes.into_inner().unwrap().into_values().collect()))
    }

    /// Longest sibling path a key is searched for (16^n hashes at most).
    pub const MAX_SEARCH_NIBBLES: usize = 6;

    /// When removing `key` from the trie at `root` collapses a branch whose other child is a
    /// hash missing from `nodes`, the path of that child.
    pub fn collapse_sibling(
        nodes: &HashMap<B256, &Bytes>,
        root: B256,
        key: B256,
    ) -> Option<Vec<u8>> {
        let path = Nibbles::unpack(key);
        let mut pos = 0;
        let mut node: Vec<u8> = nodes.get(&root)?.to_vec();
        let mut last: Option<(usize, BranchNode)> = None;
        loop {
            let next = match TrieNode::decode(&mut node.as_slice()).ok()? {
                TrieNode::Leaf(leaf) => {
                    if path.slice(pos..) != leaf.key {
                        return None;
                    }
                    break;
                }
                TrieNode::Extension(ext) => {
                    if !path.slice(pos..).starts_with(&ext.key) {
                        return None;
                    }
                    pos += ext.key.len();
                    ext.child
                }
                TrieNode::Branch(branch) => {
                    let nibble = path.get(pos)?;
                    let child = branch.as_ref().children().find(|(n, _)| *n == nibble)?.1?.clone();
                    last = Some((pos, branch));
                    pos += 1;
                    child
                }
                TrieNode::EmptyRoot => return None,
            };
            node = match next.as_hash() {
                Some(hash) => nodes.get(&hash)?.to_vec(),
                None => next.to_vec(),
            };
        }
        let (at, branch) = last?;
        let own = path.get(at)?;
        let others: Vec<(u8, alloy_trie::nodes::RlpNode)> = branch
            .as_ref()
            .children()
            .filter_map(|(n, child)| child.filter(|_| n != own).map(|c| (n, c.clone())))
            .collect();
        let [(nibble, child)] = others.as_slice() else { return None };
        let hash = child.as_hash()?;
        if nodes.contains_key(&hash) {
            return None;
        }
        let mut prefix: Vec<u8> = (0..at).filter_map(|i| path.get(i)).collect();
        prefix.push(*nibble);
        Some(prefix)
    }

    /// A storage slot whose hashed key starts with the nibbles `prefix`, found by search.
    pub fn key_with_prefix(prefix: &[u8]) -> Option<B256> {
        (0u64..1 << (4 * prefix.len() as u64 + 3)).map(|i| B256::from(U256::from(i))).find(|slot| {
            let hash = keccak256(slot);
            prefix.iter().enumerate().all(|(i, nibble)| {
                let byte = hash[i / 2];
                (if i % 2 == 0 { byte >> 4 } else { byte & 0x0f }) == *nibble
            })
        })
    }

    /// One account's proof nodes (account and storage) and code at `block`.
    fn account(
        &self,
        block: u64,
        address: Address,
        slots: &[B256],
    ) -> eyre::Result<(Vec<Bytes>, Option<Bytes>, B256)> {
        let tag = format!("{block:#x}");
        let mut nodes = Vec::new();
        let mut code_hash = KECCAK256_EMPTY;
        let mut storage_root = B256::ZERO;
        // Large slot lists go in chunks to keep responses small.
        for chunk in slots.chunks(64).map(<[B256]>::to_vec).chain(slots.is_empty().then(Vec::new)) {
            let proof: EIP1186AccountProofResponse =
                serde_json::from_value(self.call("eth_getProof", json!([address, chunk, tag]))?)?;
            code_hash = proof.code_hash;
            storage_root = proof.storage_hash;
            nodes.extend(proof.account_proof);
            for slot in proof.storage_proof {
                nodes.extend(slot.proof);
            }
        }
        let code = if code_hash == KECCAK256_EMPTY || code_hash == B256::ZERO {
            None
        } else {
            let code: Bytes =
                serde_json::from_value(self.call("eth_getCode", json!([address, tag]))?)?;
            (keccak256(&code) == code_hash).then_some(code)
        };
        Ok((nodes, code, storage_root))
    }

    /// Calls `method`, retrying quickly: most failures are a backend without the state.
    fn call(&self, method: &str, params: Value) -> eyre::Result<Value> {
        let body = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
        let mut last = String::new();
        for attempt in 0..Self::ATTEMPTS {
            let url = &self.rpc.urls[attempt as usize % self.rpc.urls.len()];
            match self.rpc.call_once(url, &body) {
                Ok(value) => return Ok(value),
                Err(error) => last = error,
            }
            thread::sleep(Duration::from_millis(if last.contains("rate limit") {
                500
            } else {
                20
            }));
        }
        Err(eyre!("{method}: no result after {} attempts: {last}", Self::ATTEMPTS))
    }
}
