//! The parent state a block executes on, behind one trait so block building and validation run
//! the same code over a stateless witness (the diff adapter) and over the node's database.

use std::{cell::RefCell, collections::HashMap, rc::Rc};

use alloy_primitives::{Address, B256, Bytes, U256};
use base_tiger_trie::{AccountInfo, AccountProof, BlockChanges, StateView};
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
    fn storage_root_after(&self, address: Address, changes: &BlockStateAccumulator) -> Result<B256, String>;
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

    fn storage_root_after(&self, address: Address, changes: &BlockStateAccumulator) -> Result<B256, String> {
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
}

impl<'d> NodeState<'d> {
    /// The state of `view`, reading block hashes from `block_hashes`.
    pub fn new(view: StateView<'d>, block_hashes: HashMap<u64, B256>) -> Self {
        Self { view, block_hashes, missing: RefCell::new(None) }
    }

    fn record(&self, error: impl ToString) -> String {
        self.missing.borrow_mut().get_or_insert_with(|| error.to_string()).clone()
    }

    /// The account at `address`.
    pub fn account(&self, address: &Address) -> Result<Option<AccountInfo>, String> {
        self.view.account(*address).map_err(|error| self.record(error))
    }

    /// The value of storage `slot` of `address`.
    pub fn storage(&self, address: &Address, slot: &U256) -> Result<U256, String> {
        self.view.storage(*address, *slot).map_err(|error| self.record(error))
    }

    /// The code with hash `hash`.
    pub fn code(&self, hash: &B256) -> Result<Bytes, String> {
        match self.view.code(*hash) {
            Ok(Some(code)) => Ok(code),
            Ok(None) => Err(self.record(format!("missing bytecode {hash}"))),
            Err(error) => Err(self.record(error)),
        }
    }

    /// The `eth_getProof` proof of `address` and `slots`.
    pub fn proof(&self, address: Address, slots: &[U256]) -> Result<AccountProof, String> {
        self.view.proof(address, slots).map_err(|error| self.record(error))
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
        self.view.state_root(Some(&block_changes(changes))).map_err(|error| self.record(error))
    }

    fn storage_root_after(&self, address: Address, changes: &BlockStateAccumulator) -> Result<B256, String> {
        self.view.storage_root_after(address, &block_changes(changes)).map_err(|error| self.record(error))
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
