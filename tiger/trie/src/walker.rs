//! The stored-trie walker: visits the branch nodes a trie keeps in its trie table and decides
//! which subtrees are unchanged, so their stored hashes can stand in for them.
//!
//! Ported from reth's `reth-trie` (`walker.rs`, `trie_cursor/subnode.rs`) and
//! `reth-trie-common` (`prefix_set.rs`) at base/reth 2b20e42 (MIT/Apache-2.0), without the
//! progress/threshold, metrics, and added-removed-keys parts tiger does not use. The behavior must
//! stay identical to reth's: base-reth reads the trie nodes tiger writes.

use alloy_primitives::B256;
use alloy_trie::{BranchNodeCompact, Nibbles};

use crate::TrieError;

/// A cursor over a stored trie table, positioned by nibble path.
pub trait TrieCursor {
    /// The node stored at exactly `key`.
    fn seek_exact(&mut self, key: Nibbles) -> Result<Option<(Nibbles, BranchNodeCompact)>, TrieError>;
    /// The first node at or after `key`.
    fn seek(&mut self, key: Nibbles) -> Result<Option<(Nibbles, BranchNodeCompact)>, TrieError>;
    /// The next node.
    fn next(&mut self) -> Result<Option<(Nibbles, BranchNodeCompact)>, TrieError>;
    /// The path the cursor is on.
    fn current(&mut self) -> Result<Option<Nibbles>, TrieError>;
}

/// The set of changed key paths, sorted. Lookups move a remembered index, so walking keys in
/// order costs amortized O(1) per lookup.
#[derive(Debug, Clone, Default)]
pub struct PrefixSet {
    /// Every key changed (a wiped storage trie).
    pub all: bool,
    /// Changed keys, sorted and unique.
    pub keys: Vec<Nibbles>,
    /// Lookup position.
    pub index: usize,
}

impl PrefixSet {
    /// A prefix set over `keys` (any order, duplicates allowed).
    pub fn new(mut keys: Vec<Nibbles>) -> Self {
        keys.sort_unstable();
        keys.dedup();
        Self { all: false, keys, index: 0 }
    }

    /// The prefix set of a wiped trie.
    pub const fn all() -> Self {
        Self { all: true, keys: Vec::new(), index: 0 }
    }

    /// Whether no key changed.
    pub fn is_empty(&self) -> bool {
        !self.all && self.keys.is_empty()
    }

    /// Whether some changed key `k` has `start <= k < end` (`end` `None`: unbounded), in
    /// nibble order, where a path sorts before every key under it.
    pub fn any_in(&self, start: &Nibbles, end: Option<&Nibbles>) -> bool {
        if self.all {
            return true;
        }
        let first = self.keys.partition_point(|key| key < start);
        self.keys.get(first).is_some_and(|key| end.is_none_or(|end| key < end))
    }

    /// Whether some changed key starts with `prefix`.
    pub fn contains(&mut self, prefix: &Nibbles) -> bool {
        if self.all {
            return true;
        }
        while self.index > 0 && &self.keys[self.index] > prefix {
            self.index -= 1;
        }
        for (offset, key) in self.keys[self.index..].iter().enumerate() {
            if key.starts_with(prefix) {
                self.index += offset;
                return true;
            }
            if key > prefix {
                self.index += offset;
                return false;
            }
        }
        false
    }
}

/// Which part of a branch node a walker stack entry points at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SubNodePosition {
    /// The branch node itself.
    ParentBranch,
    /// One of its children.
    Child(u8),
}

impl SubNodePosition {
    /// Whether this is a child position.
    pub const fn is_child(&self) -> bool {
        matches!(self, Self::Child(_))
    }

    /// Whether this is the branch itself.
    pub const fn is_parent(&self) -> bool {
        matches!(self, Self::ParentBranch)
    }

    /// The child nibble.
    pub const fn as_child(&self) -> Option<u8> {
        match self {
            Self::Child(nibble) => Some(*nibble),
            Self::ParentBranch => None,
        }
    }

    /// Whether this is the last child position.
    pub const fn is_last_child(&self) -> bool {
        match self {
            Self::ParentBranch => false,
            Self::Child(nibble) => *nibble >= 0xf,
        }
    }

    const fn increment(&mut self) {
        match self {
            Self::ParentBranch => *self = Self::Child(0),
            Self::Child(nibble) => *nibble += 1,
        }
    }
}

/// One entry of the walker stack: a stored branch node (or none) and a position in it.
#[derive(Debug, Clone)]
pub struct CursorSubNode {
    /// The branch node's path.
    pub key: Nibbles,
    /// The position.
    pub position: SubNodePosition,
    /// The stored node, if any.
    pub node: Option<BranchNodeCompact>,
    /// `key` plus the child nibble, when at a child.
    pub full_key: Nibbles,
}

impl Default for CursorSubNode {
    fn default() -> Self {
        Self::new(Nibbles::default(), None)
    }
}

impl CursorSubNode {
    /// An entry for `node` at `key`, positioned at its first child (or at itself for a root
    /// node, which carries the root hash).
    pub fn new(key: Nibbles, node: Option<BranchNodeCompact>) -> Self {
        let position = node
            .as_ref()
            .filter(|n| n.root_hash.is_none())
            .map_or(SubNodePosition::ParentBranch, |n| SubNodePosition::Child(n.state_mask.iter().next().unwrap()));
        let mut full_key = key;
        if let Some(nibble) = position.as_child() {
            full_key.push(nibble);
        }
        Self { key, position, node, full_key }
    }

    fn update_full_key(&mut self, old: SubNodePosition) {
        if let Some(nibble) = self.position.as_child() {
            if old.is_child() {
                let last = self.full_key.len() - 1;
                self.full_key.set_at(last, nibble);
            } else {
                self.full_key.push(nibble);
            }
        } else if old.is_child() {
            self.full_key.pop();
        }
    }

    /// Whether the child at the position exists (or the entry has no node).
    pub fn state_flag(&self) -> bool {
        self.node.as_ref().is_none_or(|node| self.position.as_child().is_none_or(|n| node.state_mask.is_bit_set(n)))
    }

    /// Whether the child at the position has stored nodes below it (or the entry has no node).
    pub fn tree_flag(&self) -> bool {
        self.node.as_ref().is_none_or(|node| self.position.as_child().is_none_or(|n| node.tree_mask.is_bit_set(n)))
    }

    /// Whether a hash is stored for the position.
    pub fn hash_flag(&self) -> bool {
        self.node.as_ref().is_some_and(|node| match self.position {
            SubNodePosition::ParentBranch => node.root_hash.is_some(),
            SubNodePosition::Child(nibble) => node.hash_mask.is_bit_set(nibble),
        })
    }

    /// The stored hash for the position.
    pub fn hash(&self) -> Option<B256> {
        self.node.as_ref().and_then(|node| match self.position {
            SubNodePosition::ParentBranch => node.root_hash,
            SubNodePosition::Child(nibble) => Some(node.hash_for_nibble(nibble)),
        })
    }

    /// Moves to the next position.
    pub fn inc_nibble(&mut self) {
        let old = self.position;
        self.position.increment();
        self.update_full_key(old);
    }

    /// Moves to child `nibble`.
    pub fn set_nibble(&mut self, nibble: u8) {
        let old = self.position;
        self.position = SubNodePosition::Child(nibble);
        self.update_full_key(old);
    }
}

/// Walks a stored trie in key order, skipping unchanged subtrees whose hash is stored.
#[derive(Debug)]
pub struct TrieWalker<C> {
    /// The trie table cursor.
    pub cursor: C,
    /// The path from the root to the current position.
    pub stack: Vec<CursorSubNode>,
    /// Whether the current subtree is unchanged and hashed.
    pub can_skip_current_node: bool,
    /// The changed keys.
    pub changes: PrefixSet,
    /// Stored node paths the walk found stale, when retained.
    pub removed_keys: Option<Vec<Nibbles>>,
    /// Whether the last [`Self::advance`] went through a position with no stored branch (the
    /// root, which is never stored) or found no stored branch where a mask records one, so it
    /// sought the next stored branch instead of stepping through a stored branch's masks: keys
    /// may then lie between the old and new positions that no mask records.
    pub left_stored_masks: bool,
}

impl<C: TrieCursor> TrieWalker<C> {
    /// Starts a walk at the root.
    pub fn new(cursor: C, changes: PrefixSet, retain_removed: bool) -> Result<Self, TrieError> {
        let mut this = Self {
            cursor,
            changes,
            stack: vec![CursorSubNode::default()],
            can_skip_current_node: false,
            removed_keys: retain_removed.then(Vec::new),
            left_stored_masks: false,
        };
        if let Some((key, value)) = this.node(true)? {
            this.stack[0] = CursorSubNode::new(key, Some(value));
        }
        this.update_skip_node();
        Ok(this)
    }

    /// The current path.
    pub fn key(&self) -> Option<&Nibbles> {
        self.stack.last().map(|n| &n.full_key)
    }

    /// The stored hash at the current position.
    pub fn hash(&self) -> Option<B256> {
        self.stack.last().and_then(CursorSubNode::hash)
    }

    /// Whether the current position has stored nodes below it.
    pub fn children_are_in_trie(&self) -> bool {
        self.stack.last().is_some_and(CursorSubNode::tree_flag)
    }

    /// The next key the hashed-leaf cursor must seek: the current path, or the first path after
    /// the current subtree when it is skipped.
    #[inline]
    pub fn next_unprocessed_key(&self) -> Option<(B256, Nibbles)> {
        self.key()
            .and_then(|key| if self.can_skip_current_node { key.increment() } else { Some(*key) })
            .map(|key| (B256::right_padding_from(&key.pack()), key))
    }

    fn update_skip_node(&mut self) {
        self.can_skip_current_node = match self.stack.last() {
            Some(node) => !self.changes.contains(&node.full_key) && node.hash_flag(),
            None => false,
        };
    }

    /// Moves to the next position to process. Inlined into the node iterator's loop, which
    /// thin LTO otherwise leaves to codegen-unit boundaries (0.4% of a block's instructions).
    #[inline]
    pub fn advance(&mut self) -> Result<(), TrieError> {
        self.left_stored_masks = false;
        if let Some(last) = self.stack.last() {
            if !self.can_skip_current_node && self.children_are_in_trie() {
                match last.position {
                    SubNodePosition::ParentBranch => self.move_to_next_sibling(true)?,
                    SubNodePosition::Child(_) => self.consume_node()?,
                }
            } else {
                self.move_to_next_sibling(false)?;
            }
            self.update_skip_node();
        }
        Ok(())
    }

    fn node(&mut self, exact: bool) -> Result<Option<(Nibbles, BranchNodeCompact)>, TrieError> {
        let key = *self.key().expect("key must exist");
        let entry = if exact { self.cursor.seek_exact(key)? } else { self.cursor.seek(key)? };
        if let Some((_, node)) = &entry {
            if node.state_mask.is_empty() {
                return Err(TrieError::Corrupt("stored branch node with empty state mask".into()));
            }
        }
        Ok(entry)
    }

    fn consume_node(&mut self) -> Result<(), TrieError> {
        // Seeking the next stored branch, not stepping through a stored branch's masks.
        let Some((key, node)) = self.node(false)? else {
            self.left_stored_masks = true;
            self.stack.clear();
            return Ok(());
        };
        if !key.is_empty() && !self.stack.is_empty() {
            self.stack[0].set_nibble(key.get_unchecked(0));
        }
        if let Some(subnode) = self.stack.last() {
            if !key.starts_with(&subnode.full_key) {
                self.left_stored_masks = true;
                return self.move_to_next_sibling(false);
            }
        }
        let subnode = CursorSubNode::new(key, Some(node));
        let position = subnode.position;
        self.stack.push(subnode);
        self.update_skip_node();
        if !self.can_skip_current_node || position.is_child() {
            if let Some(keys) = self.removed_keys.as_mut() {
                if let Some(current) = self.cursor.current()? {
                    keys.push(current);
                }
            }
        }
        Ok(())
    }

    /// Moves to the next sibling, popping finished nodes. A loop instead of reth's recursion.
    fn move_to_next_sibling(&mut self, mut allow_root_to_child_nibble: bool) -> Result<(), TrieError> {
        loop {
            let Some(subnode) = self.stack.last_mut() else { return Ok(()) };
            self.left_stored_masks |= subnode.node.is_none();
            if subnode.position.is_last_child() || (subnode.position.is_parent() && !allow_root_to_child_nibble) {
                self.stack.pop();
                allow_root_to_child_nibble = false;
                continue;
            }
            subnode.inc_nibble();
            if subnode.node.is_none() {
                return self.consume_node();
            }
            loop {
                if subnode.state_flag() {
                    return Ok(());
                }
                if subnode.position.is_last_child() {
                    break;
                }
                subnode.inc_nibble();
            }
            self.stack.pop();
            allow_root_to_child_nibble = false;
        }
    }
}
