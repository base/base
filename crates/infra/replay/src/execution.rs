use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
};

use alloy_consensus::{Header, Sealed};
use alloy_primitives::{Address, B256, Bytes, U256, keccak256};
use alloy_rlp::Decodable;
use alloy_rpc_types_eth::EIP1186AccountProofResponse;
use alloy_trie::Nibbles;
use base_common_chains::ChainConfig;
use base_common_evm::BaseEvmFactory;
use base_common_rpc_types_engine::BasePayloadAttributes;
use base_proof_executor::{BlockBuildingOutcome, StatelessL2Builder, TrieDBProvider};
use base_proof_mpt::{TrieHinter, TrieNode, TrieProvider};
use eyre::{Result, ensure, eyre};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{Fixture, HttpRpc, Rpc};

/// Content-addressed preimages consumed by the real stateless block executor.
/// Missing state is an error, never an empty account or zero storage fallback.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ExecutionWitness {
    /// Raw RLP trie nodes keyed by Keccak-256.
    pub nodes: BTreeMap<B256, Bytes>,
    /// Contract bytecode keyed by Keccak-256.
    pub codes: BTreeMap<B256, Bytes>,
    /// Ancestor headers used by BLOCKHASH.
    pub headers: BTreeMap<B256, Header>,
}

impl ExecutionWitness {
    /// Rejects corrupt preimages before measurements begin.
    pub fn validate(&self) -> Result<()> {
        for (hash, bytes) in self.nodes.iter().chain(&self.codes) {
            ensure!(keccak256(bytes) == *hash, "witness preimage mismatch: {hash}");
        }
        for (hash, header) in &self.headers {
            ensure!(header.hash_slow() == *hash, "witness header mismatch: {hash}");
        }
        Ok(())
    }

    /// Executes all transactions in canonical order, including system transactions, then
    /// applies the state changes and reconstructs all block commitments with production code.
    pub fn execute(&self, fixture: &Fixture) -> Result<BlockBuildingOutcome> {
        let config = ChainConfig::mainnet().rollup_config();
        let mut builder = StatelessL2Builder::new(
            &config,
            BaseEvmFactory::default(),
            self,
            self,
            Sealed::new(fixture.parent.clone()),
        );
        Ok(builder.build_block(fixture.execution_payload()?)?)
    }

    /// Checks the entire computed header, not just whether execution succeeded.
    pub fn verify(&self, fixture: &Fixture) -> Result<()> {
        self.validate()?;
        let outcome = self.execute(fixture)?;
        ensure!(
            outcome.header.inner() == &fixture.header,
            "executed header differs from mainnet: computed {:?}, expected {:?}",
            outcome.header,
            fixture.header
        );
        Ok(())
    }
}

impl TrieProvider for &ExecutionWitness {
    type Error = eyre::Report;

    fn trie_node_by_hash(&self, hash: B256) -> Result<TrieNode> {
        let bytes = self.nodes.get(&hash).ok_or_else(|| eyre!("missing trie node {hash}"))?;
        Ok(TrieNode::decode(&mut bytes.as_ref())?)
    }
}

impl TrieDBProvider for &ExecutionWitness {
    fn bytecode_by_hash(&self, hash: B256) -> Result<Bytes> {
        self.codes.get(&hash).cloned().ok_or_else(|| eyre!("missing bytecode {hash}"))
    }

    fn header_by_hash(&self, hash: B256) -> Result<Header> {
        self.headers.get(&hash).cloned().ok_or_else(|| eyre!("missing ancestor {hash}"))
    }
}

impl TrieHinter for &ExecutionWitness {
    type Error = eyre::Report;

    fn hint_trie_node(&self, _: B256) -> Result<()> {
        Ok(())
    }
    fn hint_account_proof(&self, _: Address, _: u64) -> Result<()> {
        Ok(())
    }
    fn hint_storage_proof(&self, _: Address, _: U256, _: u64) -> Result<()> {
        Ok(())
    }
    fn hint_execution_witness(&self, _: B256, _: &BasePayloadAttributes) -> Result<()> {
        Ok(())
    }
}

/// Online oracle for fixture generation only. Run on a blocking worker so the synchronous
/// executor can request read-only RPC data through the asynchronous transport.
#[derive(Debug)]
pub struct CaptureOracle {
    /// Read-only RPC transport; endpoint credentials are never serialized.
    pub rpc: RefCell<HttpRpc>,
    /// Runtime serving capture requests from the blocking worker.
    pub runtime: tokio::runtime::Handle,
    /// Every state request is pinned to this parent hash, never a moving block tag.
    pub parent: Header,
    /// Canonical child hash, used only to recover unchanged siblings after branch collapse.
    pub block_hash: B256,
    /// Accumulated, deduplicated preimages.
    pub witness: RefCell<ExecutionWitness>,
    /// Already captured account and storage paths.
    pub accesses: RefCell<BTreeMap<Address, BTreeSet<U256>>>,
    /// Account owning bytecode requested by hash.
    pub code_addresses: RefCell<BTreeMap<B256, Address>>,
    /// Paths of proof siblings that may be needed when deletions collapse a branch.
    pub sibling_paths: RefCell<BTreeMap<B256, ProofPath>>,
}

/// Location of an unopened sibling in an account or storage trie.
#[derive(Debug, Clone)]
pub struct ProofPath {
    /// Storage owner, or `None` for the account trie.
    pub address: Option<Address>,
    /// Prefix of the hashed key in nibbles, including the branch's child index.
    pub prefix: Vec<u8>,
    /// Original, unhashed key whose proof exposed this sibling.
    pub neighbor: B256,
}

impl ProofPath {
    /// Finds a probe key entering this subtree. Capture-only CPU work is bounded: deep
    /// pathological tries require a node provider rather than an unbounded preimage search.
    pub fn probe(&self) -> Result<B256> {
        const MAX_PREFIX_NIBBLES: usize = 6;
        const MAX_PROBES: u64 = 1 << 26;
        ensure!(
            !self.prefix.is_empty() && self.prefix.len() <= MAX_PREFIX_NIBBLES,
            "sibling prefix too deep for public RPC capture ({} nibbles)",
            self.prefix.len()
        );
        let mask = u32::MAX << (32 - self.prefix.len() * 4);
        let target = self
            .prefix
            .iter()
            .enumerate()
            .fold(0u32, |word, (i, nibble)| word | (u32::from(*nibble) << (28 - i * 4)));
        for value in 0..MAX_PROBES {
            let key = B256::from(U256::from(value));
            let bytes = if self.address.is_some() { key.as_slice() } else { &key[12..] };
            let hash = keccak256(bytes);
            if u32::from_be_bytes(hash[..4].try_into().expect("four bytes")) & mask == target {
                return Ok(key);
            }
        }
        Err(eyre!("sibling probe budget exhausted"))
    }
}

impl CaptureOracle {
    /// Captures the complete block witness, and refuses to export a noncanonical execution.
    pub async fn capture(fixture: Fixture, url: String) -> Result<ExecutionWitness> {
        let runtime = tokio::runtime::Handle::current();
        tokio::task::spawn_blocking(move || {
            let oracle = Self {
                rpc: RefCell::new(HttpRpc::new(&url)?),
                runtime,
                parent: fixture.parent.clone(),
                block_hash: fixture.block_hash,
                witness: RefCell::default(),
                accesses: RefCell::default(),
                code_addresses: RefCell::default(),
                sibling_paths: RefCell::default(),
            };
            let config = ChainConfig::mainnet().rollup_config();
            let mut builder = StatelessL2Builder::new(
                &config,
                BaseEvmFactory::default(),
                &oracle,
                &oracle,
                Sealed::new(fixture.parent.clone()),
            );
            let outcome = builder.build_block(fixture.execution_payload()?)?;
            ensure!(
                outcome.header.inner() == &fixture.header,
                "capture execution does not match mainnet header: {:?}",
                outcome.header
            );
            let witness = oracle.witness.into_inner();
            witness.verify(&fixture)?;
            Ok(witness)
        })
        .await?
    }

    /// Issues a read-only request from a synchronous execution callback.
    pub fn request(&self, method: &str, params: Value) -> Result<Value> {
        let mut rpc = self.rpc.borrow_mut();
        let result = self.runtime.block_on(rpc.request(method, params));
        // The witness already retains each relevant preimage; do not accumulate duplicated
        // account paths and RPC JSON for every individual storage read.
        rpc.exchanges.clear();
        result
    }

    /// Fetches only previously unseen account/storage paths.
    pub fn proof(&self, address: Address, slot: Option<U256>, block_number: u64) -> Result<()> {
        // Sealing asks for the message-passer account at the child height after applying the
        // bundle. Changed paths are already in TrieDB; any still-blinded paths are unchanged
        // parent nodes. Fetch parent proofs here as for execution, not child-state values.
        ensure!(
            block_number == self.parent.number || block_number == self.parent.number + 1,
            "unexpected proof block"
        );
        if self
            .accesses
            .borrow()
            .get(&address)
            .is_some_and(|slots| slot.is_none_or(|slot| slots.contains(&slot)))
        {
            return Ok(());
        }
        let slots: Vec<B256> = slot.map(B256::from).into_iter().collect();
        let proof: EIP1186AccountProofResponse = serde_json::from_value(self.request(
            "eth_getProof",
            json!([address, slots,
                {"blockHash": self.parent.hash_slow(), "requireCanonical": true}]),
        )?)?;
        ensure!(proof.address == address, "proof address mismatch");
        ensure!(
            proof.storage_proof.iter().map(|p| p.key.as_b256()).collect::<Vec<_>>() == slots,
            "proof slot mismatch"
        );
        let mut witness = self.witness.borrow_mut();
        for node in proof
            .account_proof
            .iter()
            .chain(proof.storage_proof.iter().flat_map(|slot| &slot.proof))
        {
            witness.nodes.insert(keccak256(node), node.clone());
        }
        drop(witness);
        self.index_proof(&proof.account_proof, address.into_word(), None)?;
        for slot in &proof.storage_proof {
            self.index_proof(&slot.proof, slot.key.as_b256(), Some(address))?;
        }
        self.code_addresses.borrow_mut().insert(proof.code_hash, address);
        self.accesses.borrow_mut().entry(address).or_default().extend(slot);
        Ok(())
    }

    /// Records sibling locations from root-to-leaf EIP-1186 proofs. No sibling value is guessed.
    pub fn index_proof(
        &self,
        proof: &[Bytes],
        neighbor: B256,
        address: Option<Address>,
    ) -> Result<()> {
        let key = Nibbles::unpack(keccak256(if address.is_some() {
            neighbor.as_slice()
        } else {
            &neighbor[12..]
        }));
        let mut depth = 0;
        for bytes in proof {
            match TrieNode::decode(&mut bytes.as_ref())? {
                TrieNode::Branch { stack } => {
                    ensure!(depth < key.len(), "invalid proof depth");
                    for (index, child) in stack.iter().enumerate() {
                        if let TrieNode::Blinded { commitment } = child {
                            let mut prefix = key.slice(..depth).to_vec();
                            prefix.push(index as u8);
                            self.sibling_paths
                                .borrow_mut()
                                .insert(*commitment, ProofPath { address, prefix, neighbor });
                        }
                    }
                    depth += 1;
                }
                TrieNode::Extension { prefix, node } => {
                    ensure!(depth + prefix.len() <= key.len(), "invalid extension depth");
                    if let TrieNode::Blinded { commitment } = *node {
                        let mut path = key.slice(..depth).to_vec();
                        path.extend_from_slice(&prefix.to_vec());
                        self.sibling_paths
                            .borrow_mut()
                            .insert(commitment, ProofPath { address, prefix: path, neighbor });
                    }
                    depth += prefix.len();
                }
                _ => break,
            }
        }
        Ok(())
    }

    /// A collapsed leaf/extension has the same value but a longer path in the child trie.
    /// Recover its old encoding from a child proof only if its hash exactly matches the
    /// missing parent commitment. Never substitute child-state values into parent state.
    pub fn collapsed_sibling(&self, path: &ProofPath, hash: B256) -> Result<bool> {
        let address = path.address.unwrap_or_else(|| Address::from_word(path.neighbor));
        let slots: Vec<B256> = path.address.map(|_| path.neighbor).into_iter().collect();
        let proof: EIP1186AccountProofResponse = serde_json::from_value(self.request(
            "eth_getProof",
            json!([address, slots, {"blockHash": self.block_hash, "requireCanonical": true}]),
        )?)?;
        for bytes in
            proof.account_proof.iter().chain(proof.storage_proof.iter().flat_map(|s| &s.proof))
        {
            let node = TrieNode::decode(&mut bytes.as_ref())?;
            let len = match &node {
                TrieNode::Leaf { prefix, .. } | TrieNode::Extension { prefix, .. } => prefix.len(),
                _ => 0,
            };
            for skip in 0..=len {
                let mut candidate = node.clone();
                match &mut candidate {
                    TrieNode::Leaf { prefix, .. } | TrieNode::Extension { prefix, .. } => {
                        *prefix = prefix.slice(skip..)
                    }
                    _ => {}
                }
                let encoded = alloy_rlp::encode(candidate);
                if keccak256(&encoded) == hash {
                    self.witness.borrow_mut().nodes.insert(hash, encoded.into());
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }
}

impl TrieProvider for &CaptureOracle {
    type Error = eyre::Report;

    fn trie_node_by_hash(&self, hash: B256) -> Result<TrieNode> {
        if !self.witness.borrow().nodes.contains_key(&hash) {
            let path = self
                .sibling_paths
                .borrow()
                .get(&hash)
                .cloned()
                .ok_or_else(|| eyre!("missing trie node with no proof path: {hash}"))?;
            if !self.collapsed_sibling(&path, hash)? {
                let key = path.probe()?;
                match path.address {
                    Some(address) => {
                        self.proof(address, Some(U256::from_be_bytes(key.0)), self.parent.number)?
                    }
                    None => self.proof(Address::from_word(key), None, self.parent.number)?,
                }
            }
        }
        (&*self.witness.borrow()).trie_node_by_hash(hash)
    }
}

impl TrieDBProvider for &CaptureOracle {
    fn bytecode_by_hash(&self, hash: B256) -> Result<Bytes> {
        if let Some(code) = self.witness.borrow().codes.get(&hash) {
            return Ok(code.clone());
        }
        let address = *self
            .code_addresses
            .borrow()
            .get(&hash)
            .ok_or_else(|| eyre!("no account for bytecode {hash}"))?;
        let code: Bytes = serde_json::from_value(self.request(
            "eth_getCode",
            json!([
                address, {"blockHash": self.parent.hash_slow(), "requireCanonical": true}
            ]),
        )?)?;
        ensure!(keccak256(&code) == hash, "code hash mismatch");
        self.witness.borrow_mut().codes.insert(hash, code.clone());
        Ok(code)
    }

    fn header_by_hash(&self, hash: B256) -> Result<Header> {
        if let Some(header) = self.witness.borrow().headers.get(&hash) {
            return Ok(header.clone());
        }
        let header: Header =
            serde_json::from_value(self.request("eth_getBlockByHash", json!([hash, false]))?)?;
        ensure!(header.hash_slow() == hash, "ancestor hash mismatch");
        self.witness.borrow_mut().headers.insert(hash, header.clone());
        Ok(header)
    }
}

impl TrieHinter for &CaptureOracle {
    type Error = eyre::Report;

    fn hint_trie_node(&self, _: B256) -> Result<()> {
        Ok(())
    }
    fn hint_account_proof(&self, address: Address, block: u64) -> Result<()> {
        self.proof(address, None, block)
    }
    fn hint_storage_proof(&self, address: Address, slot: U256, block: u64) -> Result<()> {
        self.proof(address, Some(slot), block)
    }
    fn hint_execution_witness(&self, _: B256, _: &BasePayloadAttributes) -> Result<()> {
        Ok(())
    }
}
