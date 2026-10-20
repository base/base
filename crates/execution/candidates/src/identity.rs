//! Lifecycle identity of a verified candidate group, independent of how it was submitted.

use alloy_primitives::{Address, B256, TxHash};

/// Work a group may consume over its lifetime, from its verified submission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GroupBudget {
    /// Cap on trial gas plus scoring-probe gas.
    pub max_work_gas: u64,
    /// Cap on evaluation rounds, before the server cap applies.
    pub max_evaluations: u16,
}

/// What the ledger and the pool track for a verified group: its identity, sender and nonce,
/// member hashes, expiry block and work budget.
///
/// Derive a descriptor only from a verified submission, such as
/// [`crate::CandidateGroup::descriptor`]. It carries no signature, selection rule or
/// transaction bytes, so another submission format can reuse [`crate::GroupLedger`] by
/// producing a descriptor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupDescriptor {
    /// Group identity.
    pub group_hash: B256,
    /// Shared sender.
    pub sender: Address,
    /// Shared nonce.
    pub nonce: u64,
    /// The member that is the pool and iterator lifecycle identity.
    pub representative: TxHash,
    /// Every member hash, in submission order; the index is the candidate id.
    pub sibling_hashes: Vec<TxHash>,
    /// Last block in which the group may land (inclusive). The node assigns it at admission
    /// from its configured group lifetime; the sender does not sign it.
    pub expiry_block: u64,
    /// Lifetime work budget.
    pub budget: GroupBudget,
}

impl GroupDescriptor {
    /// Returns the heap memory held by the descriptor beyond its inline size.
    pub const fn heap_size(&self) -> usize {
        core::mem::size_of_val(self.sibling_hashes.as_slice())
    }
}
