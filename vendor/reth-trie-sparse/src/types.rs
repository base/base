//! Sparse trie updates, epochs, and leaf lookup results.

use alloc::vec::Vec;
use alloy_primitives::{
    B256,
    map::{HashMap, HashSet},
};
use alloy_trie::BranchNodeCompact;
use base_execution_state_types::Nibbles;

/// Modification epoch assigned to cached sparse trie nodes.
///
/// Epochs must increase monotonically. Nodes materialized from the parent state without being
/// modified use [`Self::UNMODIFIED`].
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct TrieNodeEpoch(u64);

impl TrieNodeEpoch {
    /// Epoch assigned to nodes materialized from the parent state without being modified.
    pub const UNMODIFIED: Self = Self(0);

    /// Creates a new node modification epoch.
    pub const fn new(epoch: u64) -> Self {
        Self(epoch)
    }

    /// Returns the inner epoch.
    pub const fn get(self) -> u64 {
        self.0
    }

    /// Returns whether a node with this epoch should be pruned at the provided cutoff.
    pub const fn should_prune(self, prune_before: Self) -> bool {
        self.0 < prune_before.0
    }
}

/// Describes an update to a leaf in the sparse trie.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeafUpdate {
    /// The leaf value has been changed to the given RLP-encoded value.
    /// Empty Vec indicates the leaf has been removed.
    Changed(Vec<u8>),
    /// The leaf value may have changed, but the new value is not yet known.
    /// Used for optimistic prewarming when the actual value is unavailable.
    Touched,
}

impl LeafUpdate {
    /// Returns true if the leaf update is a change.
    pub const fn is_changed(&self) -> bool {
        matches!(self, Self::Changed(_))
    }

    /// Returns true if the leaf update is a touched update.
    pub const fn is_touched(&self) -> bool {
        matches!(self, Self::Touched)
    }
}

/// Tracks modifications to the sparse trie structure.
///
/// Maintains references to both modified and pruned/removed branches, enabling
/// one to make batch updates to a persistent database.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SparseTrieUpdates {
    /// Collection of updated intermediate nodes indexed by full path.
    pub updated_nodes: HashMap<Nibbles, BranchNodeCompact>,
    /// Collection of removed intermediate nodes indexed by full path.
    pub removed_nodes: HashSet<Nibbles>,
    /// Flag indicating whether the trie was wiped.
    pub wiped: bool,
}

impl SparseTrieUpdates {
    /// Initialize a [`Self`] with given capacities.
    pub fn with_capacity(num_updated_nodes: usize, num_removed_nodes: usize) -> Self {
        Self {
            updated_nodes: HashMap::with_capacity_and_hasher(num_updated_nodes, Default::default()),
            removed_nodes: HashSet::with_capacity_and_hasher(num_removed_nodes, Default::default()),
            wiped: false,
        }
    }
}

/// Error type for a leaf lookup operation
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeafLookupError {
    /// The path leads to a blinded node, cannot determine if leaf exists.
    /// This means the witness is not complete.
    BlindedNode {
        /// Path to the blinded node.
        path: Nibbles,
        /// Hash of the blinded node.
        hash: B256,
    },
    /// The path leads to a leaf with a different value than expected.
    /// This means the witness is malformed.
    ValueMismatch {
        /// Path to the leaf.
        path: Nibbles,
        /// Expected value.
        expected: Option<Vec<u8>>,
        /// Actual value found.
        actual: Vec<u8>,
    },
}

/// Success value for a leaf lookup operation
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeafLookup {
    /// Leaf exists with expected value.
    Exists,
    /// Leaf does not exist (exclusion proof found).
    NonExistent,
}
