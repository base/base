use alloc::collections::BTreeSet;
use alloy_primitives::{
    map::{HashMap, HashSet},
    Address, TxKind, B256,
};
use revm::context::transaction::AuthorizationTr;

use alloy_rpc_types_eth::{AccessList, AccessListItem};
use revm::{
    bytecode::opcode,
    context::JournalTr,
    context_interface::{ContextTr, Transaction},
    inspector::JournalExt,
    interpreter::{
        interpreter_types::{InputsTr, Jumps},
        Interpreter,
    },
    Inspector,
};

/// An [Inspector] that collects touched accounts and storage slots.
///
/// This can be used to construct an [AccessList] for a transaction via `eth_createAccessList`
#[derive(Debug, Default)]
pub struct AccessListInspector {
    /// All addresses that should be excluded from the final accesslist
    excluded: HashSet<Address>,
    /// All addresses and touched slots
    touched_slots: HashMap<Address, BTreeSet<B256>>,
}

impl From<AccessList> for AccessListInspector {
    fn from(access_list: AccessList) -> Self {
        Self::new(access_list)
    }
}

impl AccessListInspector {
    /// Creates a new inspector instance
    ///
    /// The `access_list` is the provided access list from the call request
    pub fn new(access_list: AccessList) -> Self {
        Self {
            excluded: Default::default(),
            touched_slots: access_list
                .0
                .into_iter()
                .map(|v| (v.address, v.storage_keys.into_iter().collect()))
                .collect(),
        }
    }

    /// Returns the excluded addresses.
    pub fn excluded(&self) -> &HashSet<Address> {
        &self.excluded
    }

    /// Returns a reference to the map of addresses and their corresponding touched storage slots.
    pub fn touched_slots(&self) -> &HashMap<Address, BTreeSet<B256>> {
        &self.touched_slots
    }

    /// Consumes the inspector and returns the map of addresses and their corresponding touched
    /// storage slots.
    pub fn into_touched_slots(self) -> HashMap<Address, BTreeSet<B256>> {
        self.touched_slots
    }

    /// Returns list of addresses and storage keys used by the transaction. It gives you the list of
    /// addresses and storage keys that were touched during execution.
    pub fn into_access_list(self) -> AccessList {
        let items = self.touched_slots.into_iter().map(|(address, slots)| AccessListItem {
            address,
            storage_keys: slots.into_iter().collect(),
        });
        AccessList(items.collect())
    }

    /// Returns list of addresses and storage keys used by the transaction. It gives you the list of
    /// addresses and storage keys that were touched during execution.
    pub fn access_list(&self) -> AccessList {
        let items = self.touched_slots.iter().map(|(address, slots)| AccessListItem {
            address: *address,
            storage_keys: slots.iter().copied().collect(),
        });
        AccessList(items.collect())
    }

    /// Collects addresses which should be excluded from the access list. Must be called before the
    /// top-level call.
    ///
    /// Those include caller, callee and precompiles.
    fn collect_excluded_addresses<CTX: ContextTr<Journal: JournalExt>>(&mut self, context: &CTX) {
        let from = context.tx().caller();
        let to = if let TxKind::Call(to) = context.tx().kind() {
            to
        } else {
            // We need to exclude the created address if this is a CREATE frame.
            //
            // This assumes that caller has already been loaded but nonce was not increased yet.
            let nonce = context.journal_ref().evm_state().get(&from).unwrap().info.nonce;
            from.create(nonce)
        };
        let precompiles = context.journal_ref().precompile_addresses().clone();

        // 7702 authorities should be excluded because those get loaded anyway
        let auth_addrs = context.tx().authorization_list().flat_map(|a| a.authority());

        self.excluded = [from, to].into_iter().chain(precompiles).chain(auth_addrs).collect();
    }
}

impl<CTX> Inspector<CTX> for AccessListInspector
where
    CTX: ContextTr<Journal: JournalExt>,
{
    fn step(&mut self, interp: &mut Interpreter, _context: &mut CTX) {
        match interp.bytecode.opcode() {
            opcode::SLOAD | opcode::SSTORE => {
                if let Ok(slot) = interp.stack.peek(0) {
                    let cur_contract = interp.input.target_address();
                    self.touched_slots
                        .entry(cur_contract)
                        .or_default()
                        .insert(B256::from(slot.to_be_bytes()));
                }
            }
            opcode::EXTCODECOPY
            | opcode::EXTCODEHASH
            | opcode::EXTCODESIZE
            | opcode::BALANCE
            | opcode::SELFDESTRUCT => {
                if let Ok(slot) = interp.stack.peek(0) {
                    let addr = Address::from_word(B256::from(slot.to_be_bytes()));
                    if !self.excluded.contains(&addr) {
                        self.touched_slots.entry(addr).or_default();
                    }
                }
            }
            opcode::DELEGATECALL | opcode::CALL | opcode::STATICCALL | opcode::CALLCODE => {
                if let Ok(slot) = interp.stack.peek(1) {
                    let addr = Address::from_word(B256::from(slot.to_be_bytes()));
                    if !self.excluded.contains(&addr) {
                        self.touched_slots.entry(addr).or_default();
                    }
                }
            }
            _ => (),
        }
    }

    fn call(
        &mut self,
        context: &mut CTX,
        _inputs: &mut revm::interpreter::CallInputs,
    ) -> Option<revm::interpreter::CallOutcome> {
        // At the top-level frame, fill the excluded addresses
        if context.journal().depth() == 0 {
            self.collect_excluded_addresses(context)
        }
        None
    }

    fn create(
        &mut self,
        context: &mut CTX,
        _inputs: &mut revm::interpreter::CreateInputs,
    ) -> Option<revm::interpreter::CreateOutcome> {
        // At the top-level frame, fill the excluded addresses
        if context.journal().depth() == 0 {
            self.collect_excluded_addresses(context)
        }
        None
    }
}
