//! Request and response types of the differential trie protocol (see `TRIE_PROTOCOL.md`).

use std::collections::BTreeMap;

use alloy_primitives::{Address, B256, Bytes, U256};
use serde::{Deserialize, Serialize};

/// One state write of a block, in hashed-key space.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "camelCase")]
pub enum TrieOp {
    /// Creates or updates the account at `key`, keeping its storage.
    #[serde(rename_all = "camelCase")]
    Account {
        /// Hashed address.
        key: B256,
        /// Account nonce.
        #[serde(with = "alloy_serde::quantity")]
        nonce: u64,
        /// Account balance.
        balance: U256,
        /// Code hash.
        code_hash: B256,
        /// Preimage of `key`, when the harness knows it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        address: Option<Address>,
        /// The code `code_hash` commits to, when the block supplies it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        code: Option<Bytes>,
    },
    /// Sets storage `slot` of the existing account at `key`; zero clears it.
    Storage {
        /// Hashed address.
        key: B256,
        /// Hashed storage slot.
        slot: B256,
        /// New value, 32 bytes big-endian.
        value: B256,
        /// Preimage of `key`, when the harness knows it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        address: Option<Address>,
        /// Preimage of `slot`, when the harness knows it.
        #[serde(default, rename = "slotKey", skip_serializing_if = "Option::is_none")]
        slot_key: Option<B256>,
    },
    /// Clears all storage of the account at `key`, which keeps existing.
    Wipe {
        /// Hashed address.
        key: B256,
        /// Preimage of `key`, when the harness knows it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        address: Option<Address>,
    },
    /// Removes the account at `key` and its storage.
    Delete {
        /// Hashed address.
        key: B256,
        /// Preimage of `key`, when the harness knows it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        address: Option<Address>,
    },
}

impl TrieOp {
    /// An account write without a preimage.
    pub const fn account(key: B256, nonce: u64, balance: U256, code_hash: B256) -> Self {
        Self::Account { key, nonce, balance, code_hash, address: None, code: None }
    }

    /// A storage write without a preimage.
    pub const fn storage(key: B256, slot: B256, value: B256) -> Self {
        Self::Storage { key, slot, value, address: None, slot_key: None }
    }

    /// A storage wipe without a preimage.
    pub const fn wipe(key: B256) -> Self {
        Self::Wipe { key, address: None }
    }

    /// An account deletion without a preimage.
    pub const fn delete(key: B256) -> Self {
        Self::Delete { key, address: None }
    }

    /// The hashed address the op writes.
    pub const fn key(&self) -> B256 {
        match self {
            Self::Account { key, .. }
            | Self::Storage { key, .. }
            | Self::Wipe { key, .. }
            | Self::Delete { key, .. } => *key,
        }
    }
}

/// Proof reads of one account and some of its slots.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProofTarget {
    /// Hashed address.
    pub key: B256,
    /// Hashed storage slots.
    #[serde(default)]
    pub slots: Vec<B256>,
    /// Preimage of `key`, when the harness knows it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<Address>,
    /// Preimages of `slots`, parallel to it (empty when `slots` is), when the harness knows
    /// them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slot_keys: Option<Vec<B256>>,
}

/// One block of a trie request.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrieBlock {
    /// Block number: the parent's plus one.
    #[serde(with = "alloy_serde::quantity")]
    pub number: u64,
    /// Blocks the chain drops before this block.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub unwind: u64,
    /// Writes that turn the tip's state back into the state the block builds on, present
    /// exactly when `unwind > 0`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub revert: Vec<TrieOp>,
    /// The block's writes, in order.
    #[serde(default)]
    pub ops: Vec<TrieOp>,
    /// Proof reads after the block.
    #[serde(default)]
    pub proofs: Vec<ProofTarget>,
}

const fn is_zero(n: &u64) -> bool {
    *n == 0
}

/// The `trie` object of a request.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrieInput {
    /// Scratch directory for the adapter's databases.
    pub dir: String,
    /// Blocks in order, from the empty state.
    pub blocks: Vec<TrieBlock>,
}

/// A trie-mode request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrieRequest {
    /// Case identifier, echoed in the response.
    pub id: String,
    /// Always `trie`.
    pub mode: String,
    /// The blocks.
    pub trie: TrieInput,
}

/// An account as a proof reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProofAccount {
    /// Nonce.
    #[serde(with = "alloy_serde::quantity")]
    pub nonce: u64,
    /// Balance.
    pub balance: U256,
    /// Code hash.
    pub code_hash: B256,
    /// Storage root.
    pub storage_root: B256,
}

/// One storage slot read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageProofResult {
    /// Hashed slot.
    pub slot: B256,
    /// Value, zero when unset.
    pub value: B256,
    /// Storage trie nodes along the slot's path.
    pub proof: Vec<Bytes>,
}

/// One account proof read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProofResult {
    /// Hashed address.
    pub key: B256,
    /// The account, `None` when it does not exist.
    pub account: Option<ProofAccount>,
    /// State trie nodes along the key's path.
    pub account_proof: Vec<Bytes>,
    /// Requested slots, in order.
    #[serde(default)]
    pub storage: Vec<StorageProofResult>,
    /// The account's code, for implementations that report it: empty for the empty code hash,
    /// absent when the implementation does not hold it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<Bytes>,
}

/// What an implementation computed after one block. Every field is optional for variants.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrieBlockResult {
    /// Block number.
    #[serde(default, skip_serializing_if = "Option::is_none", with = "alloy_serde::quantity::opt")]
    pub number: Option<u64>,
    /// State root after the block.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_root: Option<B256>,
    /// Storage roots of the existing accounts the block wrote.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_roots: Option<BTreeMap<B256, B256>>,
    /// Proof reads, in request order.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proofs: Option<Vec<ProofResult>>,
    /// A failure or inconsistency the variant found.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// A trie-mode response.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrieResponse {
    /// Echoed case identifier.
    pub id: String,
    /// `false` for an adapter-level failure.
    pub ok: bool,
    /// Failure description when `ok` is `false`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Primary implementation results, one per block.
    #[serde(default)]
    pub blocks: Vec<TrieBlockResult>,
    /// More implementations by name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub variants: BTreeMap<String, Vec<TrieBlockResult>>,
    /// Coverage features, never compared.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coverage: Option<BTreeMap<String, u64>>,
}
