//! A validated candidate group: the signed envelope plus every recovered sibling.

use alloy_consensus::{
    Transaction,
    transaction::{Recovered, TxHashRef},
};
use alloy_primitives::{Address, B256, Bytes, Signature, TxHash, U256};
use base_common_consensus::BaseTransactionSigned;

use crate::{CandidateEnvelope, FallbackMode};

/// One signed sibling and its exact EIP-2718 encoding.
#[derive(Debug, Clone)]
pub struct CandidateSibling {
    /// The recovered transaction.
    pub transaction: Recovered<BaseTransactionSigned>,
    /// The sibling's EIP-2718 encoding, as submitted.
    pub encoded: Bytes,
}

impl CandidateSibling {
    /// Returns the sibling's transaction hash.
    pub fn hash(&self) -> TxHash {
        *TxHashRef::tx_hash(self.transaction.inner())
    }

    /// Returns `value + gas_limit * max_fee_per_gas`, the most this sibling can debit its sender.
    pub fn max_cost(&self) -> U256 {
        let tx = self.transaction.inner();
        U256::from(tx.gas_limit())
            .saturating_mul(U256::from(tx.max_fee_per_gas()))
            .saturating_add(tx.value())
    }
}

/// A candidate group whose envelope, siblings and signature passed [`crate::CandidateValidator`].
///
/// The group is one pool entry, stored on its representative. Its identity is
/// [`Self::group_hash`]; the representative is only the pool and iterator lifecycle identity.
#[derive(Debug, Clone)]
pub struct CandidateGroup {
    envelope: CandidateEnvelope,
    signature: Signature,
    group_hash: B256,
    chain_id: u64,
    siblings: Vec<CandidateSibling>,
    heap_size: usize,
}

impl CandidateGroup {
    /// Assembles a group without verifying it.
    ///
    /// Use [`crate::CandidateValidator::validate`] for untrusted input; this constructor only
    /// computes the derived fields.
    pub fn new(
        envelope: CandidateEnvelope,
        signature: Signature,
        chain_id: u64,
        siblings: Vec<CandidateSibling>,
    ) -> Self {
        let group_hash = envelope.signing_hash(chain_id);
        // Computed once so the pooled entry's reported size never changes while it is pooled.
        let heap_size = core::mem::size_of_val(envelope.sibling_hashes.as_slice())
            + siblings
                .iter()
                .map(|sibling| {
                    core::mem::size_of::<CandidateSibling>()
                        + sibling.encoded.len()
                        + sibling.transaction.inner().input().len()
                })
                .sum::<usize>();
        Self { envelope, signature, group_hash, chain_id, siblings, heap_size }
    }

    /// Returns the signed envelope.
    pub const fn envelope(&self) -> &CandidateEnvelope {
        &self.envelope
    }

    /// Returns the envelope signature.
    pub const fn signature(&self) -> Signature {
        self.signature
    }

    /// Returns the EIP-712 signing hash of the envelope, the group's identity.
    pub const fn group_hash(&self) -> B256 {
        self.group_hash
    }

    /// Returns the siblings' chain ID.
    pub const fn chain_id(&self) -> u64 {
        self.chain_id
    }

    /// Returns the siblings, in envelope order.
    pub fn siblings(&self) -> &[CandidateSibling] {
        &self.siblings
    }

    /// Returns the shared sender.
    pub const fn sender(&self) -> Address {
        self.envelope.sender
    }

    /// Returns the shared nonce.
    pub const fn nonce(&self) -> u64 {
        self.envelope.nonce
    }

    /// Returns the fallback mode. Validated groups always carry a known mode.
    pub fn fallback(&self) -> FallbackMode {
        self.envelope.fallback().unwrap_or(FallbackMode::Drop)
    }

    /// Returns the sibling that is the pool and iterator lifecycle identity.
    pub fn representative(&self) -> &CandidateSibling {
        &self.siblings[self.envelope.representative_index()]
    }

    /// Returns the sibling run by `DEFAULT_SIBLING` fallback.
    pub fn default_sibling(&self) -> &CandidateSibling {
        &self.siblings[usize::from(self.envelope.default_index)]
    }

    /// Returns the index of a sibling hash, if it belongs to this group.
    pub fn sibling_index(&self, hash: &TxHash) -> Option<usize> {
        self.envelope.sibling_hashes.iter().position(|candidate| candidate == hash)
    }

    /// Returns the maximum sibling obligation, used as the group's pool cost.
    pub fn max_cost(&self) -> U256 {
        self.siblings.iter().map(CandidateSibling::max_cost).max().unwrap_or_default()
    }

    /// Returns the heap memory held by the group, fixed at construction.
    pub const fn heap_size(&self) -> usize {
        self.heap_size
    }
}
