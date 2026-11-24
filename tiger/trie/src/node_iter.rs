//! Merges the walker's skippable subtrees with hashed leaves, in key order, for the hash builder.
//!
//! Ported from reth's `reth-trie` `node_iter.rs` at base/reth 2b20e42 (MIT/Apache-2.0), without
//! metrics and the seek-caching optimizations.

use alloy_primitives::B256;
use alloy_trie::Nibbles;

use crate::{TrieCursor, TrieError, TrieWalker};

/// A cursor over hashed leaves (accounts or one account's storage), in key order.
pub trait HashedCursor {
    /// The leaf value.
    type Value: Clone;
    /// The first leaf at or after `key`.
    fn seek(&mut self, key: B256) -> Result<Option<(B256, Self::Value)>, TrieError>;
    /// The next leaf.
    fn next(&mut self) -> Result<Option<(B256, Self::Value)>, TrieError>;
}

/// What the iterator yields.
#[derive(Debug)]
pub enum TrieElement<V> {
    /// An unchanged subtree with its stored hash: path, hash, whether its children are stored.
    Branch(Nibbles, B256, bool),
    /// A leaf.
    Leaf(B256, V),
}

/// The node iterator.
#[derive(Debug)]
pub struct TrieNodeIter<C, H: HashedCursor> {
    /// The stored-trie walker.
    pub walker: TrieWalker<C>,
    /// The hashed-leaf cursor.
    pub hashed_cursor: H,
    current_hashed_entry: Option<(B256, H::Value)>,
    should_check_walker_key: bool,
}

impl<C: TrieCursor, H: HashedCursor> TrieNodeIter<C, H> {
    /// Iterates `walker`'s trie with leaves from `hashed_cursor`.
    pub const fn new(walker: TrieWalker<C>, hashed_cursor: H) -> Self {
        Self { walker, hashed_cursor, current_hashed_entry: None, should_check_walker_key: false }
    }

    /// The next element.
    pub fn try_next(&mut self) -> Result<Option<TrieElement<H::Value>>, TrieError> {
        loop {
            if let Some(key) = self.walker.key() {
                if !self.should_check_walker_key {
                    self.should_check_walker_key = true;
                    if self.walker.can_skip_current_node {
                        let hash = self.walker.hash().expect("skippable nodes have a hash");
                        return Ok(Some(TrieElement::Branch(*key, hash, self.walker.children_are_in_trie())));
                    }
                }
            }
            if let Some((hashed_key, value)) = self.current_hashed_entry.take() {
                if self.walker.key().is_some_and(|key| key < &Nibbles::unpack(hashed_key)) {
                    // As in reth, the entry is dropped: the walker's next seek finds it again.
                    self.should_check_walker_key = false;
                    continue;
                }
                self.current_hashed_entry = self.hashed_cursor.next()?;
                return Ok(Some(TrieElement::Leaf(hashed_key, value)));
            }
            let Some((seek_key, seek_prefix)) = self.walker.next_unprocessed_key() else { break };
            let can_skip_node = self.walker.can_skip_current_node;
            self.walker.advance()?;
            if can_skip_node && self.walker.key() == Some(&seek_prefix) && self.walker.children_are_in_trie() {
                self.should_check_walker_key = false;
                continue;
            }
            self.current_hashed_entry = self.hashed_cursor.seek(seek_key)?;
        }
        Ok(None)
    }
}
