//! Batch-local overlay for proofs sync execution.

use alloy_eips::{BlockNumHash, eip1898::BlockWithParent};
use reth_trie_common::{
    HashedPostState, HashedPostStateSorted,
    updates::{TrieUpdates, TrieUpdatesSorted},
};

use crate::BlockStateDiff;

/// Batch-local read overlay and pending write buffer for proofs sync.
#[derive(Debug, Default)]
pub struct ProofsBatchOverlay {
    /// Cumulative hashed post-state from prior blocks in this batch.
    read_state: HashedPostState,
    /// Cumulative trie node updates from prior blocks in this batch.
    parent_trie: TrieUpdates,
    /// Per-block diffs in append order, used at flush time.
    pending: Vec<(BlockWithParent, BlockStateDiff)>,
    /// Highest block appended to this overlay.
    last_block: Option<BlockNumHash>,
}

impl ProofsBatchOverlay {
    /// Creates an empty batch overlay.
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends a re-executed block's block-local state and trie diff.
    pub fn append_executed(
        &mut self,
        block: BlockWithParent,
        hashed_state: HashedPostState,
        trie_updates: TrieUpdates,
    ) {
        self.assert_next_block(block);

        let sorted_post_state = hashed_state.clone_into_sorted();
        let sorted_trie_updates = trie_updates.clone_into_sorted();

        self.read_state.extend(hashed_state);
        self.parent_trie.extend(trie_updates);
        self.pending.push((block, BlockStateDiff { sorted_trie_updates, sorted_post_state }));
        self.last_block = Some(block.block);
    }

    /// Appends a cached block's block-local state and trie diff.
    pub fn append_cached(
        &mut self,
        block: BlockWithParent,
        sorted_post_state: &HashedPostStateSorted,
        sorted_trie_updates: &TrieUpdatesSorted,
    ) {
        self.assert_next_block(block);

        self.read_state.extend_from_sorted(sorted_post_state);
        self.parent_trie.extend_from_sorted(sorted_trie_updates);
        self.pending.push((
            block,
            BlockStateDiff {
                sorted_trie_updates: sorted_trie_updates.clone(),
                sorted_post_state: sorted_post_state.clone(),
            },
        ));
        self.last_block = Some(block.block);
    }

    /// Returns true when no blocks have been appended.
    pub const fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    /// Returns the number of blocks appended to this overlay.
    pub const fn len(&self) -> usize {
        self.pending.len()
    }

    /// Consumes this overlay and returns the block-local diffs in append order.
    pub fn into_pending(self) -> Vec<(BlockWithParent, BlockStateDiff)> {
        self.pending
    }

    /// Returns the cumulative hashed post-state for prior blocks in this batch.
    pub const fn read_state(&self) -> &HashedPostState {
        &self.read_state
    }

    /// Returns the cumulative trie node updates for prior blocks in this batch.
    pub const fn parent_trie(&self) -> &TrieUpdates {
        &self.parent_trie
    }

    fn assert_next_block(&self, block: BlockWithParent) {
        if let Some(last_block) = self.last_block {
            debug_assert_eq!(last_block.hash, block.parent);
            debug_assert_eq!(last_block.number + 1, block.block.number);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use alloy_primitives::{B256, U256};
    use reth_primitives_traits::Account;
    use reth_trie_common::{BranchNodeCompact, HashedStorage, Nibbles, TrieMask};

    use super::*;

    fn block(number: u64) -> BlockWithParent {
        BlockWithParent::new(
            if number == 1 { B256::ZERO } else { B256::repeat_byte((number - 1) as u8) },
            alloy_eips::NumHash::new(number, B256::repeat_byte(number as u8)),
        )
    }

    fn account(nonce: u64) -> Account {
        Account { nonce, balance: U256::from(nonce), bytecode_hash: None }
    }

    fn branch(bit: u8) -> BranchNodeCompact {
        let mut state_mask = TrieMask::default();
        state_mask.set_bit(bit);
        BranchNodeCompact {
            state_mask,
            tree_mask: TrieMask::default(),
            hash_mask: TrieMask::default(),
            hashes: Arc::new(vec![]),
            root_hash: None,
        }
    }

    fn nibbles(value: u8) -> Nibbles {
        Nibbles::from_nibbles_unchecked([value])
    }

    #[test]
    fn append_executed_then_into_pending_yields_appended_diff() {
        let mut overlay = ProofsBatchOverlay::new();
        let mut state = HashedPostState::default();
        state.accounts.insert(B256::repeat_byte(0x01), Some(account(1)));
        let mut trie = TrieUpdates::default();
        trie.account_nodes.insert(nibbles(1), branch(1));

        overlay.append_executed(block(1), state, trie);

        let pending = overlay.into_pending();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].0.block.number, 1);
        assert_eq!(pending[0].1.sorted_post_state.accounts.len(), 1);
        assert_eq!(pending[0].1.sorted_trie_updates.account_nodes_ref().len(), 1);
    }

    #[test]
    fn append_cached_extends_read_state_correctly() {
        let mut overlay = ProofsBatchOverlay::new();
        let mut state = HashedPostState::default();
        let hashed_address = B256::repeat_byte(0x01);
        state.accounts.insert(hashed_address, Some(account(1)));
        let sorted_state = state.into_sorted();
        let sorted_trie = TrieUpdates::default().into_sorted();

        overlay.append_cached(block(1), &sorted_state, &sorted_trie);

        assert_eq!(overlay.read_state().accounts.get(&hashed_address), Some(&Some(account(1))));
    }

    #[test]
    fn multiple_appends_preserve_oldest_to_newest_order() {
        let mut overlay = ProofsBatchOverlay::new();
        overlay.append_executed(block(1), HashedPostState::default(), TrieUpdates::default());
        overlay.append_executed(block(2), HashedPostState::default(), TrieUpdates::default());

        let pending = overlay.into_pending();
        assert_eq!(pending[0].0.block.number, 1);
        assert_eq!(pending[1].0.block.number, 2);
    }

    #[test]
    fn read_state_destroyed_account_propagates() {
        let mut overlay = ProofsBatchOverlay::new();
        let hashed_address = B256::repeat_byte(0x01);
        let mut state = HashedPostState::default();
        state.accounts.insert(hashed_address, None);

        overlay.append_executed(block(1), state, TrieUpdates::default());

        assert_eq!(overlay.read_state().accounts.get(&hashed_address), Some(&None));
    }

    #[test]
    fn read_state_wiped_storage_sticky() {
        let mut overlay = ProofsBatchOverlay::new();
        let hashed_address = B256::repeat_byte(0x01);
        let hashed_slot = B256::repeat_byte(0x02);
        let mut wiped = HashedPostState::default();
        wiped.storages.insert(hashed_address, HashedStorage::new(true));
        let mut write = HashedPostState::default();
        write.storages.insert(
            hashed_address,
            HashedStorage::from_iter(false, [(hashed_slot, U256::from(7))]),
        );

        overlay.append_executed(block(1), wiped, TrieUpdates::default());
        overlay.append_executed(block(2), write, TrieUpdates::default());

        let storage = overlay.read_state().storages.get(&hashed_address).unwrap();
        assert!(storage.wiped);
        assert_eq!(storage.storage.get(&hashed_slot), Some(&U256::from(7)));
    }

    #[test]
    fn parent_trie_updates_accumulate_with_latest_wins() {
        let mut overlay = ProofsBatchOverlay::new();
        let path = nibbles(1);
        let mut first = TrieUpdates::default();
        first.account_nodes.insert(path, branch(1));
        let mut second = TrieUpdates::default();
        second.account_nodes.insert(path, branch(2));

        overlay.append_executed(block(1), HashedPostState::default(), first);
        overlay.append_executed(block(2), HashedPostState::default(), second);

        assert_eq!(overlay.parent_trie().account_nodes.get(&path), Some(&branch(2)));
    }

    #[test]
    fn len_and_is_empty_track_pending() {
        let mut overlay = ProofsBatchOverlay::new();
        assert!(overlay.is_empty());
        assert_eq!(overlay.len(), 0);

        overlay.append_executed(block(1), HashedPostState::default(), TrieUpdates::default());

        assert!(!overlay.is_empty());
        assert_eq!(overlay.len(), 1);
    }

    #[test]
    fn pending_keeps_each_block_diff_local() {
        let mut overlay = ProofsBatchOverlay::new();
        let mut first = HashedPostState::default();
        first.accounts.insert(B256::repeat_byte(0x01), Some(account(1)));
        let mut second = HashedPostState::default();
        second.accounts.insert(B256::repeat_byte(0x02), Some(account(2)));

        overlay.append_executed(block(1), first, TrieUpdates::default());
        overlay.append_executed(block(2), second, TrieUpdates::default());

        let pending = overlay.into_pending();
        assert_eq!(pending[1].1.sorted_post_state.accounts.len(), 1);
        assert_eq!(pending[1].1.sorted_post_state.accounts[0].0, B256::repeat_byte(0x02));
    }
}
