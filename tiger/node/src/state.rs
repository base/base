//! The parent state a block executes on, behind one trait so block building and validation run
//! the same code over a stateless witness (the diff adapter) and over the node's database.

use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, BTreeSet, HashMap},
    rc::Rc,
};

use alloy_primitives::{Address, B256, Bytes, U256};
use base_tiger_trie::{
    AccountInfo, AccountProof, BlockChanges, PreparedRoot, StateView, TrieError,
};
use evm2::{
    AccountInfo as EvmAccount, BlockStateAccumulator,
    bytecode::Bytecode,
    evm::{Database, Db, DynDatabase},
};

use crate::{WitnessDb, WitnessState};

/// The state before a block: reads for the EVM and roots after the block's changes.
pub trait ParentState {
    /// The EVM's view of this state.
    fn database(&self) -> Box<dyn DynDatabase + '_>;
    /// The state root after `changes`.
    fn state_root_after(&self, changes: &BlockStateAccumulator) -> Result<B256, String>;
    /// The storage root of `address` after `changes`.
    fn storage_root_after(
        &self,
        address: Address,
        changes: &BlockStateAccumulator,
    ) -> Result<B256, String>;
    /// The first read this state could not serve (a witness or partial state missing data).
    fn missing(&self) -> Option<String>;
}

impl ParentState for Rc<WitnessState> {
    fn database(&self) -> Box<dyn DynDatabase + '_> {
        Box::new(Db::new(WitnessDb(Rc::clone(self))))
    }

    fn state_root_after(&self, changes: &BlockStateAccumulator) -> Result<B256, String> {
        WitnessState::state_root_after(self, changes).map_err(|error| error.to_string())
    }

    fn storage_root_after(
        &self,
        address: Address,
        changes: &BlockStateAccumulator,
    ) -> Result<B256, String> {
        WitnessState::storage_root_after(self, address, changes).map_err(|error| error.to_string())
    }

    fn missing(&self) -> Option<String> {
        WitnessState::missing(self).map(|error| error.to_string())
    }
}

/// A block's state changes in the state database's form.
pub fn block_changes(changes: &BlockStateAccumulator) -> BlockChanges {
    let mut out = BlockChanges::default();
    for (address, account) in changes.accounts_sorted() {
        let info = account.current.as_ref().map(|info| AccountInfo {
            nonce: info.nonce,
            balance: info.balance,
            code_hash: info.code_hash,
        });
        out.accounts.push((address, info));
    }
    out.wiped = changes.storage_wipes_sorted();
    for (key, slot) in changes.storage() {
        out.storage.push((key.address(), key.key(), slot.current));
    }
    out.storage.sort_by_key(|(address, slot, _)| (*address, *slot));
    for (hash, code) in changes.code() {
        let bytes = code.original_bytes();
        if !bytes.is_empty() {
            out.codes.push((*hash, bytes));
        }
    }
    out
}

/// The node's state after one stored block: a database view at a canonical block plus the
/// in-memory blocks of a side branch on top, and the block hashes `BLOCKHASH` reads.
///
/// It holds an MDBX read transaction: keep it for one request or one block's execution.
#[derive(Debug)]
pub struct NodeState<'d> {
    view: StateView<'d>,
    block_hashes: HashMap<u64, B256>,
    missing: RefCell<Option<String>>,
    /// Whether a read failed for state not held locally (a partial, stateless datadir).
    unknown: Cell<bool>,
    /// The trie changes of the last `state_root_after`, for the commit to reuse.
    prepared: RefCell<Option<PreparedRoot>>,
    /// When collecting ([`Self::collecting`]): the state reads that were not local.
    misses: Option<RefCell<Misses>>,
}

/// The accounts, storage slots and code a block's execution read that a partial state does not
/// hold, collected so they can be fetched in one round.
#[derive(Debug, Default)]
pub struct Misses {
    /// Accounts.
    pub accounts: BTreeSet<Address>,
    /// Storage slots by account.
    pub storage: BTreeMap<Address, BTreeSet<U256>>,
    /// Code, by hash, with an account that holds it.
    pub codes: BTreeMap<B256, Option<Address>>,
    /// When the state root missed a trie node: the accounts the block deleted (with no slots)
    /// and the storage slots it set to zero, by account.
    pub deleted: BTreeMap<Address, BTreeSet<U256>>,
    /// An account read with each code hash, to fetch the code of.
    code_owners: HashMap<B256, Address>,
}

impl Misses {
    /// Whether nothing was missed by key (a missing trie node or block hash is not a key).
    pub fn is_empty(&self) -> bool {
        self.accounts.is_empty() && self.storage.is_empty() && self.codes.is_empty()
    }
}

impl<'d> NodeState<'d> {
    /// The state of `view`, reading block hashes from `block_hashes`.
    pub fn new(view: StateView<'d>, block_hashes: HashMap<u64, B256>) -> Self {
        Self {
            view,
            block_hashes,
            missing: RefCell::new(None),
            unknown: Cell::new(false),
            prepared: RefCell::new(None),
            misses: None,
        }
    }

    /// This state with reads of state that is not local answered as absent (zero, empty) and
    /// recorded in [`Self::take_misses`] instead of failing, so one execution finds many of the
    /// keys a block needs. The execution still reports the first miss as missing state.
    pub fn collecting(mut self) -> Self {
        self.misses = Some(RefCell::new(Misses::default()));
        self
    }

    /// The reads collected as missing (empty when not collecting).
    pub fn take_misses(&self) -> Misses {
        let mut misses = self
            .misses
            .as_ref()
            .map(|misses| std::mem::take(&mut *misses.borrow_mut()))
            .unwrap_or_default();
        for (hash, owner) in &mut misses.codes {
            *owner = misses.code_owners.get(hash).copied();
        }
        misses
    }

    /// Records `miss` when collecting and `error` is state that is not local.
    fn collect(&self, error: &TrieError, miss: impl FnOnce(&mut Misses)) -> bool {
        match &self.misses {
            Some(misses) if error.is_unknown() => {
                miss(&mut misses.borrow_mut());
                true
            }
            _ => false,
        }
    }

    fn record(&self, error: impl ToString) -> String {
        self.missing.borrow_mut().get_or_insert_with(|| error.to_string()).clone()
    }

    fn record_trie(&self, error: TrieError) -> String {
        if error.is_unknown() {
            self.unknown.set(true);
        }
        self.record(error)
    }

    /// Whether a failed read needed state this datadir does not hold (a stateless node's
    /// partial state), rather than failing on unreadable data.
    pub fn is_unknown(&self) -> bool {
        self.unknown.get()
    }

    /// The account at `address`.
    pub fn account(&self, address: &Address) -> Result<Option<AccountInfo>, String> {
        match self.view.account(*address) {
            Ok(account) => {
                if let (Some(misses), Some(account)) = (&self.misses, &account) {
                    misses.borrow_mut().code_owners.insert(account.code_hash, *address);
                }
                Ok(account)
            }
            Err(error) if self.collect(&error, |misses| _ = misses.accounts.insert(*address)) => {
                self.record_trie(error);
                Ok(None)
            }
            Err(error) => Err(self.record_trie(error)),
        }
    }

    /// The value of storage `slot` of `address`.
    pub fn storage(&self, address: &Address, slot: &U256) -> Result<U256, String> {
        match self.view.storage(*address, *slot) {
            Ok(value) => Ok(value),
            Err(error)
                if self.collect(&error, |misses| {
                    _ = misses.storage.entry(*address).or_default().insert(*slot)
                }) =>
            {
                self.record_trie(error);
                Ok(U256::ZERO)
            }
            Err(error) => Err(self.record_trie(error)),
        }
    }

    /// The code with hash `hash`.
    pub fn code(&self, hash: &B256) -> Result<Bytes, String> {
        match self.view.code(*hash) {
            Ok(Some(code)) => Ok(code),
            Ok(None) => {
                self.unknown.set(true);
                let error = self.record(format!("missing bytecode {hash}"));
                match &self.misses {
                    Some(misses) => {
                        misses.borrow_mut().codes.insert(*hash, None);
                        Ok(Bytes::new())
                    }
                    None => Err(error),
                }
            }
            Err(error) if self.collect(&error, |misses| _ = misses.codes.insert(*hash, None)) => {
                self.record_trie(error);
                Ok(Bytes::new())
            }
            Err(error) => Err(self.record_trie(error)),
        }
    }

    /// The `eth_getProof` proof of `address` and `slots`.
    pub fn proof(&self, address: Address, slots: &[U256]) -> Result<AccountProof, String> {
        self.view.proof(address, slots).map_err(|error| self.record_trie(error))
    }

    /// The trie changes behind the last state root computed after a block on this state, which
    /// [`base_tiger_trie::StateDb::commit_prepared`] reuses when the block is committed.
    pub fn take_prepared(&self) -> Option<PreparedRoot> {
        self.prepared.borrow_mut().take()
    }

    /// The hash of ancestor block `number`.
    pub fn block_hash(&self, number: u64) -> Result<B256, String> {
        self.block_hashes.get(&number).copied().ok_or_else(|| self.record(format!("missing block hash {number}")))
    }
}

impl ParentState for NodeState<'_> {
    fn database(&self) -> Box<dyn DynDatabase + '_> {
        Box::new(Db::new(NodeDb(self)))
    }

    fn state_root_after(&self, changes: &BlockStateAccumulator) -> Result<B256, String> {
        let changes = block_changes(changes);
        let prepared = match self.view.prepare_root(&changes) {
            Ok(prepared) => prepared,
            Err(error) => {
                // A deletion can collapse a branch into a sibling no read loaded; the deleted
                // keys let the caller find that sibling in the block's post-state proofs.
                if let (Some(misses), TrieError::MissingNode { .. }) = (&self.misses, &error) {
                    let mut misses = misses.borrow_mut();
                    for (address, account) in &changes.accounts {
                        if account.is_none() {
                            misses.deleted.entry(*address).or_default();
                        }
                    }
                    for (address, slot, value) in &changes.storage {
                        if value.is_zero() {
                            misses.deleted.entry(*address).or_default().insert(*slot);
                        }
                    }
                }
                return Err(self.record(error));
            }
        };
        let root = prepared.root;
        *self.prepared.borrow_mut() = Some(prepared);
        Ok(root)
    }

    fn storage_root_after(
        &self,
        address: Address,
        changes: &BlockStateAccumulator,
    ) -> Result<B256, String> {
        // The message passer's account is read only for its storage root, not by execution.
        self.view.storage_root_after(address, &block_changes(changes)).map_err(|error| {
            self.collect(&error, |misses| _ = misses.accounts.insert(address));
            self.record(error)
        })
    }

    fn missing(&self) -> Option<String> {
        self.missing.borrow().clone()
    }
}

/// An error reading the node's state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateReadError(pub String);

impl core::fmt::Display for StateReadError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for StateReadError {}

/// The evm2 database over a [`NodeState`].
#[derive(Debug)]
pub struct NodeDb<'s, 'd>(pub &'s NodeState<'d>);

impl Database for NodeDb<'_, '_> {
    type Error = StateReadError;

    fn get_account(&mut self, address: &Address) -> Result<Option<EvmAccount>, StateReadError> {
        Ok(self.0.account(address).map_err(StateReadError)?.map(|account| EvmAccount {
            balance: account.balance,
            nonce: account.nonce,
            code_hash: account.code_hash,
            code: None,
            ..EvmAccount::default()
        }))
    }

    fn get_code_by_hash(&mut self, code_hash: &B256) -> Result<Bytecode, StateReadError> {
        Ok(Bytecode::new_raw(self.0.code(code_hash).map_err(StateReadError)?))
    }

    fn get_storage(&mut self, address: &Address, key: &U256) -> Result<U256, StateReadError> {
        self.0.storage(address, key).map_err(StateReadError)
    }

    fn get_block_hash(&mut self, number: &U256) -> Result<B256, StateReadError> {
        self.0.block_hash(number.saturating_to()).map_err(StateReadError)
    }
}
