//! In-memory world state of a synthetic chain and its stateless witness.

use std::collections::{BTreeMap, BTreeSet};

use alloy_primitives::{Address, B256, Bytes, KECCAK256_EMPTY, U256, keccak256};
use alloy_trie::TrieAccount;
use eyre::{bail, eyre};

use crate::{PostAccount, Witness, r#gen::trie::TrieBuilder};

/// One account of the world state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    /// Nonce.
    pub nonce: u64,
    /// Balance.
    pub balance: U256,
    /// Code hash (`KECCAK256_EMPTY` for accounts without code).
    pub code_hash: B256,
    /// Non-zero storage slots.
    pub storage: BTreeMap<B256, U256>,
}

impl Default for Account {
    fn default() -> Self {
        Self { nonce: 0, balance: U256::ZERO, code_hash: KECCAK256_EMPTY, storage: BTreeMap::new() }
    }
}

/// The full state of a synthetic chain at one block.
///
/// `codes` is a library of every bytecode the generator knows, keyed by hash; it may hold code
/// that no account currently uses (for example the runtime of a contract a pending transaction
/// will deploy), so that post-state diffs, which carry only code hashes, can be resolved.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorldState {
    /// Accounts by address.
    pub accounts: BTreeMap<Address, Account>,
    /// Known bytecodes by hash.
    pub codes: BTreeMap<B256, Bytes>,
}

impl WorldState {
    /// Registers a bytecode in the code library and returns its hash.
    pub fn register_code(&mut self, code: Bytes) -> B256 {
        let hash = keccak256(&code);
        if hash != KECCAK256_EMPTY {
            self.codes.entry(hash).or_insert(code);
        }
        hash
    }

    /// Sets the code of an account, creating it if needed.
    pub fn set_code(&mut self, address: Address, code: Bytes) {
        let hash = self.register_code(code);
        self.accounts.entry(address).or_default().code_hash = hash;
    }

    /// Adds `amount` to an account's balance, creating it if needed.
    pub fn fund(&mut self, address: Address, amount: U256) {
        let account = self.accounts.entry(address).or_default();
        account.balance = account.balance.saturating_add(amount);
    }

    /// The account at `address`, if it exists.
    pub fn account(&self, address: &Address) -> Option<&Account> {
        self.accounts.get(address)
    }

    /// The nonce of `address` (0 for missing accounts).
    pub fn nonce(&self, address: &Address) -> u64 {
        self.accounts.get(address).map_or(0, |account| account.nonce)
    }

    /// The balance of `address` (0 for missing accounts).
    pub fn balance(&self, address: &Address) -> U256 {
        self.accounts.get(address).map_or(U256::ZERO, |account| account.balance)
    }

    /// The code of `address`, if it has any.
    pub fn code(&self, address: &Address) -> Option<&Bytes> {
        let account = self.accounts.get(address)?;
        self.codes.get(&account.code_hash)
    }

    /// A storage slot of `address`.
    pub fn storage(&self, address: &Address, slot: B256) -> U256 {
        self.accounts.get(address).and_then(|a| a.storage.get(&slot).copied()).unwrap_or_default()
    }

    /// Computes the state root and a complete witness: every trie node stored by hash (account
    /// trie and storage tries) and the code of every account that has code.
    pub fn witness(&self) -> (B256, Witness) {
        let mut builder = TrieBuilder::default();
        let mut leaves = Vec::with_capacity(self.accounts.len());
        let mut code_hashes = BTreeSet::new();
        for (address, account) in &self.accounts {
            let storage_root = builder.build(
                account
                    .storage
                    .iter()
                    .map(|(slot, value)| (keccak256(slot), alloy_rlp::encode(value)))
                    .collect(),
            );
            let trie_account =
                TrieAccount::new(account.nonce, account.balance, storage_root, account.code_hash);
            leaves.push((keccak256(address), alloy_rlp::encode(trie_account)));
            if account.code_hash != KECCAK256_EMPTY {
                code_hashes.insert(account.code_hash);
            }
        }
        let root = builder.build(leaves);
        let codes = code_hashes.iter().filter_map(|hash| self.codes.get(hash).cloned()).collect();
        (root, Witness { state: builder.nodes, codes })
    }

    /// Applies a response's post-state diff.
    ///
    /// Fails when the diff references a code hash missing from the code library, since the next
    /// witness could not include that code.
    pub fn apply_post(
        &mut self,
        post: &BTreeMap<Address, Option<PostAccount>>,
    ) -> eyre::Result<()> {
        for (address, change) in post {
            let Some(change) = change else {
                self.accounts.remove(address);
                continue;
            };
            if change.code_hash != KECCAK256_EMPTY
                && change.code_hash != B256::ZERO
                && !self.codes.contains_key(&change.code_hash)
            {
                bail!("post state of {address} has unknown code hash {}", change.code_hash);
            }
            let account = self.accounts.entry(*address).or_default();
            account.nonce = change.nonce;
            account.balance = change.balance;
            account.code_hash =
                if change.code_hash == B256::ZERO { KECCAK256_EMPTY } else { change.code_hash };
            for (slot, value) in &change.storage {
                let value = U256::from_be_bytes(value.0);
                if value.is_zero() {
                    account.storage.remove(slot);
                } else {
                    account.storage.insert(*slot, value);
                }
            }
        }
        Ok(())
    }

    /// Checks that the state root after applying a diff equals the root the clients computed.
    pub fn check_root(&self, expected: B256) -> eyre::Result<()> {
        let (root, _) = self.witness();
        if root != expected {
            return Err(eyre!("state root {root} after applying post does not match {expected}"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use alloy_primitives::{Address, B256, Bytes, KECCAK256_EMPTY, U256, keccak256};
    use alloy_trie::{
        TrieAccount,
        root::{state_root_unhashed, storage_root_unhashed},
    };

    use super::WorldState;
    use crate::PostAccount;

    #[test]
    fn root_matches_alloy_trie_and_post_applies() {
        let mut state = WorldState::default();
        for i in 1..40u8 {
            let address = Address::repeat_byte(i);
            state.fund(address, U256::from(i) << 70);
            if i % 3 == 0 {
                state.set_code(address, Bytes::from(vec![0x60, i, 0x00]));
                let account = state.accounts.get_mut(&address).unwrap();
                for slot in 0..u64::from(i) {
                    account.storage.insert(B256::from(U256::from(slot)), U256::from(slot + 1));
                }
            }
        }
        let expected = state_root_unhashed(state.accounts.iter().map(|(address, account)| {
            let storage_root =
                storage_root_unhashed(account.storage.iter().map(|(slot, value)| (*slot, *value)));
            (
                *address,
                TrieAccount::new(account.nonce, account.balance, storage_root, account.code_hash),
            )
        }));
        let (root, witness) = state.witness();
        assert_eq!(root, expected);
        assert_eq!(witness.codes.len(), 13);

        let mut post = BTreeMap::new();
        post.insert(Address::repeat_byte(1), None);
        post.insert(
            Address::repeat_byte(3),
            Some(PostAccount {
                nonce: 5,
                balance: U256::from(9),
                code_hash: keccak256([0x60, 3, 0x00]),
                storage: [(B256::from(U256::from(0)), B256::ZERO)].into_iter().collect(),
            }),
        );
        state.apply_post(&post).unwrap();
        assert!(state.account(&Address::repeat_byte(1)).is_none());
        assert_eq!(state.nonce(&Address::repeat_byte(3)), 5);
        assert_eq!(state.storage(&Address::repeat_byte(3), B256::ZERO), U256::ZERO);
        assert_eq!(state.account(&Address::repeat_byte(2)).unwrap().code_hash, KECCAK256_EMPTY);

        let mut unknown = BTreeMap::new();
        unknown.insert(
            Address::repeat_byte(2),
            Some(PostAccount {
                nonce: 0,
                balance: U256::ZERO,
                code_hash: B256::repeat_byte(9),
                storage: BTreeMap::new(),
            }),
        );
        assert!(state.apply_post(&unknown).is_err());
    }
}
