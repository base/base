//! A block's hashed-key writes as reth hashed post states.

use alloy_primitives::{
    B256, U256,
    map::{B256Map, B256Set},
};
use alloy_trie::KECCAK_EMPTY;
use base_diff_fuzz::TrieOp;
use reth_primitives_traits::Account;
use reth_trie_common::HashedPostState;

/// A block's writes as reth sees them: the hashed post state of each transaction-sized chunk, in
/// order, and the block's whole hashed post state.
///
/// A new chunk starts at every op that writes a key or slot the current chunk already wrote, so
/// a value written and set back within the block reaches the engine's sparse trie as two
/// updates, as two transactions would. As in the node, there is no storage wipe flag: wiping or
/// deleting an account writes zero to every slot it has in the database and every slot the block
/// wrote before.
#[derive(Debug, Default)]
pub struct BlockChanges {
    /// Per-chunk hashed post states.
    pub chunks: Vec<HashedPostState>,
    /// The whole block. Every account with storage changes also has an account entry, as in a
    /// hashed post state built from a bundle.
    pub state: HashedPostState,
    /// Every account key the block wrote, sorted.
    pub written: Vec<B256>,
}

impl BlockChanges {
    /// The account an `account` op sets.
    pub fn account(nonce: u64, balance: U256, code_hash: B256) -> Account {
        Account { nonce, balance, bytecode_hash: (code_hash != KECCAK_EMPTY).then_some(code_hash) }
    }

    /// Splits `ops` into chunks. `slots` lists an account's storage slots and `account` reads an
    /// account, both in the state before the block.
    pub fn new<'a>(
        ops: impl IntoIterator<Item = &'a TrieOp>,
        slots: impl Fn(&B256) -> Vec<B256>,
        account: impl Fn(&B256) -> Option<Account>,
    ) -> Self {
        let mut out = Self::default();
        let mut chunk = HashedPostState::default();
        let mut chunk_keys: B256Set = B256Set::default();
        let mut chunk_slots: B256Map<B256Set> = B256Map::default();
        let mut written_slots: B256Map<B256Set> = B256Map::default();
        for op in ops {
            let key = op.key();
            let repeat = match op {
                TrieOp::Storage { slot, .. } => {
                    chunk_slots.get(&key).is_some_and(|slots| slots.contains(slot))
                        || chunk_keys.contains(&key)
                }
                _ => chunk_keys.contains(&key) || chunk_slots.contains_key(&key),
            };
            if repeat {
                out.chunks.push(core::mem::take(&mut chunk));
                chunk_keys.clear();
                chunk_slots.clear();
            }
            match op {
                TrieOp::Account { nonce, balance, code_hash, .. } => {
                    chunk.accounts.insert(key, Some(Self::account(*nonce, *balance, *code_hash)));
                    chunk_keys.insert(key);
                }
                TrieOp::Storage { slot, value, .. } => {
                    let value = U256::from_be_bytes(value.0);
                    chunk.storages.entry(key).or_default().storage.insert(*slot, value);
                    chunk_slots.entry(key).or_default().insert(*slot);
                    written_slots.entry(key).or_default().insert(*slot);
                }
                TrieOp::Wipe { .. } | TrieOp::Delete { .. } => {
                    let storage = &mut chunk.storages.entry(key).or_default().storage;
                    let earlier = written_slots.get(&key).into_iter().flatten().copied();
                    for slot in slots(&key).into_iter().chain(earlier) {
                        storage.insert(slot, U256::ZERO);
                    }
                    if matches!(op, TrieOp::Delete { .. }) {
                        chunk.accounts.insert(key, None);
                    }
                    chunk_keys.insert(key);
                }
            }
        }
        out.chunks.push(chunk);
        out.chunks.retain(|chunk| !chunk.is_empty());
        for chunk in &out.chunks {
            out.state.extend_ref(chunk);
        }
        let storage_only: Vec<B256> = out
            .state
            .storages
            .keys()
            .filter(|key| !out.state.accounts.contains_key(*key))
            .copied()
            .collect();
        for key in storage_only {
            out.state.accounts.insert(key, account(&key));
        }
        out.written = out.state.accounts.keys().copied().collect();
        out.written.sort();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrites_split_chunks_and_wipes_zero_old_and_new_slots() {
        let key = B256::repeat_byte(1);
        let (a, b, c) = (B256::repeat_byte(0xa), B256::repeat_byte(0xb), B256::repeat_byte(0xc));
        let ops = [
            TrieOp::storage(key, b, B256::with_last_byte(1)),
            TrieOp::storage(key, b, B256::ZERO),
            TrieOp::wipe(key),
            TrieOp::storage(key, c, B256::with_last_byte(2)),
        ];
        let account = BlockChanges::account(1, U256::ZERO, KECCAK_EMPTY);
        let changes = BlockChanges::new(&ops, |_| vec![a], |_| Some(account));
        // The reset of `b`, the wipe after writing `b`, and the write after the wipe each start a
        // chunk.
        assert_eq!(changes.chunks.len(), 4);
        let storage = &changes.state.storages[&key].storage;
        assert_eq!(storage[&a], U256::ZERO);
        assert_eq!(storage[&b], U256::ZERO);
        assert_eq!(storage[&c], U256::from(2));
        assert_eq!(changes.state.accounts[&key], Some(account));
    }
}
