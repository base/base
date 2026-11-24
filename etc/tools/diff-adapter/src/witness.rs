//! A reth [`StateProvider`] served from a stateless execution witness.

use std::{
    collections::{BTreeSet, HashMap},
    sync::{Arc, Mutex, MutexGuard},
};

use alloy_consensus::constants::KECCAK_EMPTY;
use alloy_primitives::{
    Address, B256, BlockNumber, Bytes, StorageKey, StorageValue, U256, keccak256,
    map::{B256Map, B256Set},
};
use alloy_rlp::Decodable;
use alloy_trie::{Nibbles, TrieAccount};
use base_proof_mpt::{TrieNode, TrieNodeError, TrieProvider};
use reth_primitives_traits::{Account, Bytecode};
use reth_storage_api::{
    AccountReader, BlockHashReader, BytecodeReader, HashedPostStateProvider, StateProofProvider,
    StateProvider, StateRootProvider, StorageRootProvider,
};
use reth_storage_errors::provider::{ProviderError, ProviderResult};
use reth_trie_common::{
    AccountProof, DecodedMultiProofV2, ExecutionWitnessMode, HashedPostState, HashedStorage,
    KeccakKeyHasher, MultiProof, MultiProofTargets, MultiProofTargetsV2, StorageMultiProof,
    StorageProof, TrieInput, updates::TrieUpdates,
};
use revm::database::BundleState;

/// Data the witness does not contain, or cannot provide.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WitnessError {
    /// A trie node on the path to a read or written key is not in `witness.state`.
    #[error("missing trie node {0} in witness")]
    MissingTrieNode(B256),
    /// A bytecode the block loads is not in `witness.codes`.
    #[error("missing bytecode {0} in witness")]
    MissingCode(B256),
    /// `BLOCKHASH` asked for a block that is not among the request's ancestors.
    #[error("missing ancestor header for block {0}")]
    MissingBlockHash(BlockNumber),
    /// A witness trie node or leaf could not be decoded.
    #[error("malformed witness trie: {0}")]
    Malformed(String),
    /// The execution path asked for a capability a witness cannot serve.
    #[error("witness state does not support {0}")]
    Unsupported(&'static str),
}

/// Trie node preimages keyed by their keccak hash.
///
/// The first missing preimage is remembered, because the trie and EVM error types it passes
/// through flatten it into a string.
#[derive(Debug, Default)]
pub struct TrieNodes {
    /// Node preimages by `keccak256(rlp(node))`.
    pub nodes: B256Map<Bytes>,
    /// The first read that the witness could not serve.
    pub missing: Mutex<Option<WitnessError>>,
}

impl TrieNodes {
    /// Records `error` as the reason this request cannot be answered, keeping the first one.
    pub fn record_missing(&self, error: WitnessError) -> WitnessError {
        let mut missing = self.missing.lock().unwrap_or_else(|e| e.into_inner());
        missing.get_or_insert(error).clone()
    }

    /// Converts a trie error, preferring the recorded missing node over the flattened text.
    pub fn trie_error(&self, error: TrieNodeError) -> WitnessError {
        self.missing
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .unwrap_or_else(|| WitnessError::Malformed(error.to_string()))
    }

    /// Reads the account at `hashed_address` from the account trie rooted at `trie`.
    pub fn open_account(
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

    /// Applies hashed slot updates to the storage trie `trie` and returns its new root.
    pub fn apply_storage(
        &self,
        mut trie: TrieNode,
        updates: &HashedStorage,
    ) -> Result<B256, WitnessError> {
        let mut slots = updates.storage.iter().collect::<Vec<_>>();
        slots.sort_unstable_by_key(|(slot, _)| **slot);
        for (slot, value) in slots {
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
        Ok(trie.blind())
    }
}

impl TrieProvider for TrieNodes {
    type Error = WitnessError;

    fn trie_node_by_hash(&self, key: B256) -> Result<TrieNode, Self::Error> {
        let Some(node) = self.nodes.get(&key) else {
            return Err(self.record_missing(WitnessError::MissingTrieNode(key)));
        };
        TrieNode::decode(&mut node.as_ref())
            .map_err(|error| WitnessError::Malformed(format!("node {key}: {error}")))
    }
}

/// The opened part of the parent state, cached across reads.
///
/// Nodes are revealed lazily from [`TrieNodes`] and never modified: post-state roots are computed
/// on clones.
#[derive(Debug, Clone)]
pub struct OpenTries {
    /// Account trie rooted at the parent state root.
    pub accounts: TrieNode,
    /// Storage tries by hashed address, rooted at the parent storage roots.
    pub storages: B256Map<TrieNode>,
    /// Hashed addresses whose storage the last hashed post state wiped: accounts destroyed and
    /// recreated in one block, before Cancun. The pinned reth `HashedStorage` has no wipe flag,
    /// so the provider records it from the bundle.
    pub wiped: B256Set,
}

/// Shared witness data behind a [`WitnessState`].
#[derive(Debug)]
pub struct WitnessStore {
    /// Trie node preimages.
    pub nodes: TrieNodes,
    /// Bytecodes by code hash.
    pub codes: B256Map<Bytes>,
    /// Ancestor block hashes by number, served to `BLOCKHASH`.
    pub block_hashes: HashMap<BlockNumber, B256>,
    /// Revealed trie nodes.
    pub tries: Mutex<OpenTries>,
}

/// A [`StateProvider`] for the parent state of a block, served from its execution witness.
///
/// Reads walk the account and storage tries from the parent state root, revealing witness nodes
/// on the way. A read whose path leaves the witness fails with [`WitnessError::MissingTrieNode`]
/// instead of defaulting, and the error is kept in [`WitnessState::missing`]. Roots are computed
/// over the partial trie by applying the hashed post state to it.
#[derive(Debug, Clone)]
pub struct WitnessState {
    /// Shared witness data; clones read the same revealed tries.
    pub store: Arc<WitnessStore>,
}

impl WitnessState {
    /// Creates the parent state rooted at `state_root`.
    ///
    /// `storage_roots` are parent storage roots known without reading the account trie, such as
    /// the `L2ToL1MessagePasser` root an Isthmus parent header commits to as its withdrawals root.
    /// The node reads that root from its database even when the block does not touch the
    /// account, so `debug_executionWitness` does not include the account's trie path.
    pub fn new(
        state_root: B256,
        nodes: &[Bytes],
        codes: &[Bytes],
        block_hashes: impl IntoIterator<Item = (BlockNumber, B256)>,
        storage_roots: impl IntoIterator<Item = (Address, B256)>,
    ) -> Self {
        let nodes = nodes.iter().map(|node| (keccak256(node), node.clone())).collect();
        let codes = codes.iter().map(|code| (keccak256(code), code.clone())).collect();
        let block_hashes = block_hashes.into_iter().collect();
        Self {
            store: Arc::new(WitnessStore {
                nodes: TrieNodes { nodes, missing: Mutex::new(None) },
                codes,
                block_hashes,
                tries: Mutex::new(OpenTries {
                    accounts: TrieNode::new_blinded(state_root),
                    storages: storage_roots
                        .into_iter()
                        .map(|(address, root)| (keccak256(address), TrieNode::new_blinded(root)))
                        .collect(),
                    wiped: B256Set::default(),
                }),
            }),
        }
    }

    /// The first read the witness could not serve, if any.
    pub fn missing(&self) -> Option<WitnessError> {
        self.store.nodes.missing.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Locks the revealed tries.
    pub fn tries(&self) -> MutexGuard<'_, OpenTries> {
        self.store.tries.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Returns the parent-state storage trie of `hashed_address`, or `None` if the account does
    /// not exist.
    pub fn storage_trie(
        &self,
        tries: &mut OpenTries,
        hashed_address: B256,
    ) -> Result<Option<TrieNode>, WitnessError> {
        if let Some(trie) = tries.storages.get(&hashed_address) {
            return Ok(Some(trie.clone()));
        }
        let Some(account) = self.store.nodes.open_account(&mut tries.accounts, hashed_address)?
        else {
            return Ok(None);
        };
        let trie = TrieNode::new_blinded(account.storage_root);
        tries.storages.insert(hashed_address, trie.clone());
        Ok(Some(trie))
    }

    /// Computes the state root of the parent state with `post` applied.
    pub fn compute_state_root(&self, post: &HashedPostState) -> Result<B256, WitnessError> {
        let mut guard = self.tries();
        let tries = &mut *guard;
        let nodes = &self.store.nodes;
        let mut accounts = tries.accounts.clone();

        let hashed_addresses =
            post.accounts.keys().chain(post.storages.keys()).copied().collect::<BTreeSet<_>>();
        for hashed_address in hashed_addresses {
            let path = Nibbles::unpack(hashed_address);
            let parent = nodes.open_account(&mut accounts, hashed_address)?;
            let account = post
                .accounts
                .get(&hashed_address)
                .map_or_else(|| parent.map(Account::from), |account| *account);
            let Some(account) = account else {
                match accounts.delete(&path, nodes) {
                    Ok(()) | Err(TrieNodeError::KeyNotFound) => {}
                    Err(error) => return Err(nodes.trie_error(error)),
                }
                continue;
            };

            let parent_storage_root =
                parent.map_or(alloy_trie::EMPTY_ROOT_HASH, |a| a.storage_root);
            let storage_root = match post.storages.get(&hashed_address) {
                // A wiped storage restarts from an empty trie, not the parent's.
                Some(updates) if tries.wiped.contains(&hashed_address) => nodes
                    .apply_storage(TrieNode::new_blinded(alloy_trie::EMPTY_ROOT_HASH), updates)?,
                None if tries.wiped.contains(&hashed_address) => alloy_trie::EMPTY_ROOT_HASH,
                Some(updates) => {
                    let trie = tries
                        .storages
                        .get(&hashed_address)
                        .cloned()
                        .unwrap_or_else(|| TrieNode::new_blinded(parent_storage_root));
                    nodes.apply_storage(trie, updates)?
                }
                None => parent_storage_root,
            };
            let leaf = alloy_rlp::encode(account.into_trie_account(storage_root));
            accounts.insert(&path, leaf.into(), nodes).map_err(|error| nodes.trie_error(error))?;
        }
        Ok(accounts.blind())
    }

    /// Computes the storage root of `address` in the parent state with `updates` applied.
    pub fn compute_storage_root(
        &self,
        address: Address,
        updates: &HashedStorage,
    ) -> Result<B256, WitnessError> {
        let mut guard = self.tries();
        let trie = self
            .storage_trie(&mut guard, keccak256(address))?
            .unwrap_or_else(|| TrieNode::new_blinded(alloy_trie::EMPTY_ROOT_HASH));
        self.store.nodes.apply_storage(trie, updates)
    }

    /// Fails a provider call the witness cannot serve.
    pub fn unsupported<T>(what: &'static str) -> ProviderResult<T> {
        Err(ProviderError::other(WitnessError::Unsupported(what)))
    }
}

impl BlockHashReader for WitnessState {
    fn block_hash(&self, number: BlockNumber) -> ProviderResult<Option<B256>> {
        self.store.block_hashes.get(&number).map(|hash| Some(*hash)).ok_or_else(|| {
            ProviderError::other(
                self.store.nodes.record_missing(WitnessError::MissingBlockHash(number)),
            )
        })
    }

    fn canonical_hashes_range(
        &self,
        start: BlockNumber,
        end: BlockNumber,
    ) -> ProviderResult<Vec<B256>> {
        (start..end).map(|number| self.block_hash(number).map(Option::unwrap_or_default)).collect()
    }
}

impl AccountReader for WitnessState {
    fn basic_account(&self, address: &Address) -> ProviderResult<Option<Account>> {
        let mut tries = self.tries();
        let account = self
            .store
            .nodes
            .open_account(&mut tries.accounts, keccak256(address))
            .map_err(ProviderError::other)?;
        Ok(account.map(Account::from))
    }
}

impl BytecodeReader for WitnessState {
    fn bytecode_by_hash(&self, code_hash: &B256) -> ProviderResult<Option<Bytecode>> {
        if *code_hash == KECCAK_EMPTY {
            return Ok(Some(Bytecode::new_raw(Bytes::new())));
        }
        self.store
            .codes
            .get(code_hash)
            .map(|code| Some(Bytecode::new_raw(code.clone())))
            .ok_or_else(|| {
                ProviderError::other(
                    self.store.nodes.record_missing(WitnessError::MissingCode(*code_hash)),
                )
            })
    }
}

impl StateProvider for WitnessState {
    fn storage(
        &self,
        account: Address,
        storage_key: StorageKey,
    ) -> ProviderResult<Option<StorageValue>> {
        let mut guard = self.tries();
        let tries = &mut *guard;
        let hashed_address = keccak256(account);
        let nodes = &self.store.nodes;
        if !tries.storages.contains_key(&hashed_address) {
            let trie = self.storage_trie(tries, hashed_address).map_err(ProviderError::other)?;
            if trie.is_none() {
                return Ok(None);
            }
        }
        let trie = tries.storages.get_mut(&hashed_address).expect("storage trie opened above");
        let leaf = trie
            .open(&Nibbles::unpack(keccak256(storage_key)), nodes)
            .map_err(|error| ProviderError::other(nodes.trie_error(error)))?;
        leaf.map(|rlp| {
            U256::decode(&mut rlp.as_ref())
                .map_err(|error| ProviderError::other(WitnessError::Malformed(error.to_string())))
        })
        .transpose()
    }
}

impl StateRootProvider for WitnessState {
    fn state_root(&self, hashed_state: HashedPostState) -> ProviderResult<B256> {
        self.compute_state_root(&hashed_state).map_err(ProviderError::other)
    }

    fn state_root_from_nodes(&self, _input: TrieInput) -> ProviderResult<B256> {
        Self::unsupported("state_root_from_nodes")
    }

    fn state_root_with_updates(
        &self,
        hashed_state: HashedPostState,
    ) -> ProviderResult<(B256, TrieUpdates)> {
        Ok((self.state_root(hashed_state)?, TrieUpdates::default()))
    }

    fn state_root_from_nodes_with_updates(
        &self,
        _input: TrieInput,
    ) -> ProviderResult<(B256, TrieUpdates)> {
        Self::unsupported("state_root_from_nodes_with_updates")
    }
}

impl StorageRootProvider for WitnessState {
    fn storage_root(
        &self,
        address: Address,
        hashed_storage: HashedStorage,
    ) -> ProviderResult<B256> {
        self.compute_storage_root(address, &hashed_storage).map_err(ProviderError::other)
    }

    fn storage_proof(
        &self,
        _address: Address,
        _slot: B256,
        _hashed_storage: HashedStorage,
    ) -> ProviderResult<StorageProof> {
        Self::unsupported("storage_proof")
    }

    fn storage_multiproof(
        &self,
        _address: Address,
        _slots: &[B256],
        _hashed_storage: HashedStorage,
    ) -> ProviderResult<StorageMultiProof> {
        Self::unsupported("storage_multiproof")
    }
}

impl StateProofProvider for WitnessState {
    fn proof(
        &self,
        _input: TrieInput,
        _address: Address,
        _slots: &[B256],
    ) -> ProviderResult<AccountProof> {
        Self::unsupported("proof")
    }

    fn multiproof(
        &self,
        _input: TrieInput,
        _targets: MultiProofTargets,
    ) -> ProviderResult<MultiProof> {
        Self::unsupported("multiproof")
    }

    fn multiproof_v2(
        &self,
        _targets: MultiProofTargetsV2,
        _requires_masks: bool,
    ) -> ProviderResult<DecodedMultiProofV2> {
        Self::unsupported("multiproof_v2")
    }

    fn witness(
        &self,
        _input: TrieInput,
        _target: HashedPostState,
        _mode: ExecutionWitnessMode,
    ) -> ProviderResult<Vec<Bytes>> {
        Self::unsupported("witness")
    }
}

impl HashedPostStateProvider for WitnessState {
    fn hashed_post_state(&self, bundle_state: &BundleState) -> ProviderResult<HashedPostState> {
        self.tries().wiped = bundle_state
            .state()
            .iter()
            .filter(|(_, account)| account.was_destroyed() && account.info.is_some())
            .map(|(address, _)| keccak256(address))
            .collect();
        Ok(HashedPostState::from_bundle_state::<KeccakKeyHasher>(bundle_state.state()))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use alloy_trie::{HashBuilder, proof::ProofRetainer};

    use super::*;

    /// Builds the trie of `leaves` and returns its root and every node preimage.
    fn trie(leaves: &BTreeMap<B256, Vec<u8>>) -> (B256, Vec<Bytes>) {
        let targets = leaves.keys().map(Nibbles::unpack).collect();
        let mut builder = HashBuilder::default().with_proof_retainer(ProofRetainer::new(targets));
        for (key, value) in leaves {
            builder.add_leaf(Nibbles::unpack(key), value);
        }
        let root = builder.root();
        (root, builder.take_proof_nodes().into_inner().into_values().collect())
    }

    /// A small complete state: accounts by address with storage slots by slot key.
    #[derive(Clone, Default)]
    struct World(BTreeMap<Address, (Account, BTreeMap<B256, U256>)>);

    impl World {
        fn root_and_nodes(&self) -> (B256, Vec<Bytes>) {
            let mut nodes = Vec::new();
            let mut leaves = BTreeMap::new();
            for (address, (account, storage)) in &self.0 {
                let slots = storage
                    .iter()
                    .filter(|(_, value)| !value.is_zero())
                    .map(|(slot, value)| (keccak256(slot), alloy_rlp::encode(value)))
                    .collect();
                let (storage_root, storage_nodes) = trie(&slots);
                nodes.extend(storage_nodes);
                leaves.insert(
                    keccak256(address),
                    alloy_rlp::encode(account.into_trie_account(storage_root)),
                );
            }
            let (root, account_nodes) = trie(&leaves);
            nodes.extend(account_nodes);
            (root, nodes)
        }
    }

    fn account(nonce: u64, balance: u64) -> Account {
        Account::new(nonce, U256::from(balance), None)
    }

    fn world() -> World {
        let mut world = World::default();
        for i in 1..=6u8 {
            let storage = (0..u64::from(i % 3))
                .map(|slot| (B256::from(U256::from(slot)), U256::from(u64::from(i) * 7 + slot)))
                .collect();
            world.0.insert(Address::repeat_byte(i), (account(u64::from(i), 1_000), storage));
        }
        world
    }

    #[test]
    fn reads_and_post_state_root_match_a_full_trie() {
        let before = world();
        let (root, nodes) = before.root_and_nodes();
        let state = WitnessState::new(root, &nodes, &[], [], []);

        let a2 = Address::repeat_byte(2);
        assert_eq!(state.basic_account(&a2).unwrap(), Some(account(2, 1_000)));
        assert_eq!(state.storage(a2, B256::from(U256::from(1))).unwrap(), Some(U256::from(15)));
        assert_eq!(state.storage(a2, B256::from(U256::from(9))).unwrap(), None);
        assert_eq!(state.basic_account(&Address::repeat_byte(0xee)).unwrap(), None);

        // Change a balance, clear and add slots, delete an account, create one with storage.
        let mut after = before;
        let mut post = HashedPostState::default();
        let changed = after.0.get_mut(&a2).unwrap();
        changed.0.balance = U256::from(5);
        changed.1.insert(B256::from(U256::from(0)), U256::ZERO);
        changed.1.insert(B256::from(U256::from(4)), U256::from(44));
        post.accounts.insert(keccak256(a2), Some(changed.0));
        post.storages.insert(
            keccak256(a2),
            HashedStorage {
                storage: [
                    (keccak256(B256::from(U256::from(0))), U256::ZERO),
                    (keccak256(B256::from(U256::from(4))), U256::from(44)),
                ]
                .into_iter()
                .collect(),
            },
        );
        let deleted = Address::repeat_byte(4);
        after.0.remove(&deleted);
        post.accounts.insert(keccak256(deleted), None);
        let created = Address::repeat_byte(0xee);
        let slot = B256::repeat_byte(0x11);
        after.0.insert(created, (account(1, 0), [(slot, U256::from(1))].into_iter().collect()));
        post.accounts.insert(keccak256(created), Some(account(1, 0)));
        post.storages.insert(
            keccak256(created),
            HashedStorage { storage: [(keccak256(slot), U256::from(1))].into_iter().collect() },
        );

        assert_eq!(state.state_root(post).unwrap(), after.root_and_nodes().0);
        assert_eq!(state.missing(), None);
    }

    #[test]
    fn destroyed_and_recreated_account_restarts_from_empty_storage() {
        // Account 2 holds slots 0 and 1. In one block it self-destructs (pre-Cancun) and is
        // recreated at the same address with only slot 7. Slot 1 must not survive.
        let before = world();
        let (root, nodes) = before.root_and_nodes();
        let state = WitnessState::new(root, &nodes, &[], [], []);
        let a2 = Address::repeat_byte(2);
        let recreated = account(1, 0);
        let slot = U256::from(7);

        let mut after = before;
        after.0.insert(a2, (recreated, [(B256::from(slot), U256::from(70))].into_iter().collect()));

        let mut bundle = BundleState::default();
        bundle.state.insert(
            a2,
            revm::database::BundleAccount {
                info: Some(recreated.into()),
                original_info: Some(account(2, 1_000).into()),
                storage: [(
                    slot,
                    revm::database::states::StorageSlot::new_changed(U256::ZERO, U256::from(70)),
                )]
                .into_iter()
                .collect(),
                status: revm::database::AccountStatus::DestroyedChanged,
            },
        );
        let post = state.hashed_post_state(&bundle).unwrap();

        assert_eq!(state.state_root(post).unwrap(), after.root_and_nodes().0);
    }

    #[test]
    fn missing_node_fails_the_read_and_is_recorded() {
        let (root, nodes) = world().root_and_nodes();
        let state = WitnessState::new(root, &nodes[..0], &[], [], []);
        assert!(state.basic_account(&Address::repeat_byte(1)).is_err());
        assert_eq!(state.missing(), Some(WitnessError::MissingTrieNode(root)));
    }
}
