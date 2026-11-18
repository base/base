//! The harness's own model of trie state: applies ops, and computes roots and proofs from
//! scratch with [`TrieBuilder`], independently of every client.

use std::collections::{BTreeMap, HashMap};

use alloy_primitives::{B256, Bytes, U256, keccak256};
use alloy_rlp::{Encodable, Header};
use alloy_trie::{EMPTY_ROOT_HASH, KECCAK_EMPTY, TrieAccount};

use crate::{
    ProofAccount, ProofResult, ProofTarget, StorageProofResult, TrieBlockResult, TrieBuilder,
    TrieOp,
};

/// An account of a [`TrieModel`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelAccount {
    /// Nonce.
    pub nonce: u64,
    /// Balance.
    pub balance: U256,
    /// Code hash.
    pub code_hash: B256,
    /// Nonzero storage slots.
    pub storage: BTreeMap<B256, U256>,
}

/// Account and storage state in hashed-key space.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrieModel {
    /// Existing accounts by hashed address.
    pub accounts: BTreeMap<B256, ModelAccount>,
}

/// A trie built from scratch: its root and every node by hash.
#[derive(Debug, Default)]
pub struct BuiltTrie {
    /// Root hash.
    pub root: B256,
    /// Nodes stored by hash.
    pub nodes: HashMap<B256, Bytes>,
}

impl BuiltTrie {
    /// Builds the trie of `(key, RLP value)` leaves.
    pub fn build(leaves: Vec<(B256, Vec<u8>)>) -> Self {
        let mut builder = TrieBuilder::default();
        let root = builder.build(leaves);
        let nodes = builder.nodes.into_iter().map(|node| (keccak256(&node), node)).collect();
        Self { root, nodes }
    }

    /// The nodes along `key`'s path from the root, embedded ones included, and the leaf value
    /// when the key is present.
    pub fn proof(&self, key: &B256) -> (Vec<Bytes>, Option<Vec<u8>>) {
        let mut out = Vec::new();
        if self.root == EMPTY_ROOT_HASH {
            return (out, None);
        }
        let nibbles: Vec<u8> = key.iter().flat_map(|b| [b >> 4, b & 0x0f]).collect();
        let mut node = self.nodes.get(&self.root).expect("the root is stored").to_vec();
        let mut depth = 0;
        loop {
            out.push(Bytes::from(node.clone()));
            let items = Self::items(&node);
            let next = if items.len() == 17 {
                let child = items[nibbles[depth] as usize];
                depth += 1;
                child
            } else {
                let path = Self::string(items[0]);
                let leaf = path[0] >> 4 >= 2;
                let mut rest: Vec<u8> = path[1..].iter().flat_map(|b| [b >> 4, b & 0x0f]).collect();
                if (path[0] >> 4) & 1 == 1 {
                    rest.insert(0, path[0] & 0x0f);
                }
                if nibbles[depth..].get(..rest.len()) != Some(rest.as_slice()) {
                    return (out, None);
                }
                depth += rest.len();
                if leaf {
                    return (out, Some(Self::string(items[1]).to_vec()));
                }
                items[1]
            };
            node = match next {
                [] | [alloy_rlp::EMPTY_STRING_CODE] => return (out, None),
                [0xa0, hash @ ..] => {
                    self.nodes.get(&B256::from_slice(hash)).expect("child stored").to_vec()
                }
                inline => inline.to_vec(),
            };
        }
    }

    /// The raw RLP items of a list node.
    fn items(node: &[u8]) -> Vec<&[u8]> {
        let mut buf = node;
        let header = Header::decode(&mut buf).expect("a node is an RLP list");
        let mut payload = &buf[..header.payload_length];
        let mut items = Vec::new();
        while !payload.is_empty() {
            let mut probe = payload;
            let item = Header::decode(&mut probe).expect("node items are RLP");
            let len = payload.len() - probe.len() + item.payload_length;
            items.push(&payload[..len]);
            payload = &payload[len..];
        }
        items
    }

    /// The payload of an RLP string item.
    fn string(item: &[u8]) -> &[u8] {
        let mut buf = item;
        let header = Header::decode(&mut buf).expect("an RLP string");
        &buf[..header.payload_length]
    }
}

impl TrieModel {
    /// Applies one op.
    pub fn apply(&mut self, op: &TrieOp) {
        match op {
            TrieOp::Account { key, nonce, balance, code_hash, .. } => {
                let account = self.accounts.entry(*key).or_insert_with(|| ModelAccount {
                    nonce: 0,
                    balance: U256::ZERO,
                    code_hash: KECCAK_EMPTY,
                    storage: BTreeMap::new(),
                });
                account.nonce = *nonce;
                account.balance = *balance;
                account.code_hash = *code_hash;
            }
            TrieOp::Storage { key, slot, value, .. } => {
                let account = self.accounts.get_mut(key).expect("storage of an existing account");
                let value = U256::from_be_bytes(value.0);
                if value.is_zero() {
                    account.storage.remove(slot);
                } else {
                    account.storage.insert(*slot, value);
                }
            }
            TrieOp::Wipe { key, .. } => {
                if let Some(account) = self.accounts.get_mut(key) {
                    account.storage.clear();
                }
            }
            TrieOp::Delete { key, .. } => {
                self.accounts.remove(key);
            }
        }
    }

    /// The storage trie of an account (empty when it does not exist).
    pub fn storage_trie(&self, key: &B256) -> BuiltTrie {
        let leaves = self.accounts.get(key).map_or_else(Vec::new, |account| {
            account.storage.iter().map(|(slot, value)| (*slot, alloy_rlp::encode(value))).collect()
        });
        BuiltTrie::build(leaves)
    }

    /// The account trie, and the storage root of every account.
    pub fn state_trie(&self) -> (BuiltTrie, BTreeMap<B256, B256>) {
        let roots: BTreeMap<B256, B256> =
            self.accounts.keys().map(|key| (*key, self.storage_trie(key).root)).collect();
        let leaves = self
            .accounts
            .iter()
            .map(|(key, account)| {
                let leaf = TrieAccount {
                    nonce: account.nonce,
                    balance: account.balance,
                    storage_root: roots[key],
                    code_hash: account.code_hash,
                };
                let mut rlp = Vec::new();
                leaf.encode(&mut rlp);
                (*key, rlp)
            })
            .collect();
        (BuiltTrie::build(leaves), roots)
    }

    /// Ops that turn `self` into `target`: deletes, account writes, and per-slot storage writes.
    pub fn diff_to(&self, target: &Self) -> Vec<TrieOp> {
        let mut ops = Vec::new();
        for key in self.accounts.keys().filter(|key| !target.accounts.contains_key(*key)) {
            ops.push(TrieOp::delete(*key));
        }
        for (key, want) in &target.accounts {
            let have = self.accounts.get(key);
            if have.is_none_or(|have| {
                (have.nonce, have.balance, have.code_hash)
                    != (want.nonce, want.balance, want.code_hash)
            }) {
                ops.push(TrieOp::account(*key, want.nonce, want.balance, want.code_hash));
            }
            let empty = BTreeMap::new();
            let have_storage = have.map_or(&empty, |have| &have.storage);
            for slot in have_storage.keys().filter(|slot| !want.storage.contains_key(*slot)) {
                ops.push(TrieOp::storage(*key, *slot, B256::ZERO));
            }
            for (slot, value) in &want.storage {
                if have_storage.get(slot) != Some(value) {
                    ops.push(TrieOp::storage(*key, *slot, B256::from(value.to_be_bytes::<32>())));
                }
            }
        }
        ops
    }

    /// The result an implementation should report after a block that wrote `written` keys and
    /// reads `proofs`.
    pub fn result(&self, number: u64, written: &[B256], proofs: &[ProofTarget]) -> TrieBlockResult {
        let (state, roots) = self.state_trie();
        let storage_roots =
            written.iter().filter_map(|key| roots.get(key).map(|root| (*key, *root))).collect();
        let proofs = proofs
            .iter()
            .map(|target| {
                let (account_proof, _) = state.proof(&target.key);
                let account = self.accounts.get(&target.key);
                let storage_trie = self.storage_trie(&target.key);
                let storage = target
                    .slots
                    .iter()
                    .map(|slot| {
                        let (proof, _) = storage_trie.proof(slot);
                        let value = account
                            .and_then(|account| account.storage.get(slot))
                            .copied()
                            .unwrap_or_default();
                        StorageProofResult {
                            slot: *slot,
                            value: B256::from(value.to_be_bytes::<32>()),
                            proof,
                        }
                    })
                    .collect();
                ProofResult {
                    key: target.key,
                    account: account.map(|account| ProofAccount {
                        nonce: account.nonce,
                        balance: account.balance,
                        code_hash: account.code_hash,
                        storage_root: roots[&target.key],
                    }),
                    account_proof,
                    storage,
                }
            })
            .collect();
        TrieBlockResult {
            number: Some(number),
            state_root: Some(state.root),
            storage_roots: Some(storage_roots),
            proofs: Some(proofs),
            error: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_trie::{HashBuilder, Nibbles, proof::verify_proof};

    use super::*;

    fn key(prefix: &[u8]) -> B256 {
        let mut key = B256::repeat_byte(0x11);
        key[..prefix.len()].copy_from_slice(prefix);
        key
    }

    fn reference_root(model: &TrieModel) -> B256 {
        let mut builder = HashBuilder::default();
        for (key, account) in &model.accounts {
            let mut storage = HashBuilder::default();
            for (slot, value) in &account.storage {
                storage.add_leaf(Nibbles::unpack(slot), &alloy_rlp::encode(value));
            }
            let leaf = TrieAccount {
                nonce: account.nonce,
                balance: account.balance,
                storage_root: storage.root(),
                code_hash: account.code_hash,
            };
            builder.add_leaf(Nibbles::unpack(key), &alloy_rlp::encode(leaf));
        }
        builder.root()
    }

    #[test]
    fn roots_and_proofs_of_shared_prefixes_and_embedded_nodes() {
        let mut model = TrieModel::default();
        let accounts = [key(&[0x12, 0x34]), key(&[0x12, 0x35]), key(&[0x12]), key(&[0xab])];
        for (i, account) in accounts.iter().enumerate() {
            model.apply(&TrieOp::account(*account, i as u64, U256::from(7), KECCAK_EMPTY));
        }
        // Slots that share 63 nibbles: their leaves are tiny and embed in their branch.
        let mut slot_a = B256::ZERO;
        slot_a[31] = 0x01;
        let mut slot_b = B256::ZERO;
        slot_b[31] = 0x02;
        for slot in [slot_a, slot_b] {
            model.apply(&TrieOp::storage(accounts[0], slot, B256::with_last_byte(1)));
        }
        let result = model.result(
            1,
            &accounts,
            &[ProofTarget { key: accounts[0], slots: vec![slot_a], ..Default::default() }],
        );
        let root = result.state_root.unwrap();
        assert_eq!(root, reference_root(&model));

        let proof = &result.proofs.as_ref().unwrap()[0];
        let leaf = alloy_rlp::encode(TrieAccount {
            nonce: 0,
            balance: U256::from(7),
            storage_root: proof.account.as_ref().unwrap().storage_root,
            code_hash: KECCAK_EMPTY,
        });
        verify_proof(root, Nibbles::unpack(accounts[0]), Some(leaf), &proof.account_proof).unwrap();
        // The storage proof holds the root branch and the embedded leaf.
        let storage = &proof.storage[0];
        assert!(storage.proof.iter().skip(1).any(|node| node.len() < 32));
        let hashed: Vec<Bytes> = storage
            .proof
            .iter()
            .enumerate()
            .filter(|(i, node)| *i == 0 || node.len() >= 32)
            .map(|(_, node)| node.clone())
            .collect();
        let storage_root = proof.account.as_ref().unwrap().storage_root;
        verify_proof(storage_root, Nibbles::unpack(slot_a), Some(vec![1]), &hashed).unwrap();

        // An absent key next to existing ones proves exclusion.
        let absent = key(&[0x12, 0x36]);
        let (nodes, value) = model.state_trie().0.proof(&absent);
        assert!(value.is_none());
        verify_proof(root, Nibbles::unpack(absent), None, &nodes).unwrap();
    }

    #[test]
    fn diff_to_restores_an_earlier_state() {
        let mut before = TrieModel::default();
        let a = key(&[1]);
        let b = key(&[2]);
        before.apply(&TrieOp::account(a, 1, U256::ZERO, KECCAK_EMPTY));
        before.apply(&TrieOp::storage(a, B256::ZERO, B256::with_last_byte(5)));
        let mut after = before.clone();
        after.apply(&TrieOp::delete(a));
        after.apply(&TrieOp::account(a, 2, U256::ZERO, KECCAK_EMPTY));
        after.apply(&TrieOp::account(b, 1, U256::ZERO, KECCAK_EMPTY));
        after.apply(&TrieOp::storage(b, B256::ZERO, B256::with_last_byte(9)));
        let mut restored = after.clone();
        for op in after.diff_to(&before) {
            restored.apply(&op);
        }
        assert_eq!(restored, before);
    }
}
