//! The executable reference specification of the Merkle Patricia state root and of the branch
//! nodes reth stores for it.
//!
//! This is the definition the optimized calculators ([`crate::TrieView`] and [`crate::LiveTrie`])
//! are tested and proved against. It favors being obviously correct over being fast: every node
//! is built from scratch by recursion over a sorted key set, as the Yellow Paper (appendix D)
//! defines the trie, with no caching, no incremental state and no shared code with the
//! calculators (its own hex-prefix encoding and node layout; RLP headers and keccak come from
//! `alloy-rlp` and `alloy-primitives`).
//!
//! Keys are 64-nibble hashed keys, so no key is a prefix of another and branches carry no value.
//!
//! The stored nodes are the branch nodes reth keeps in `AccountsTrie` / `StoragesTrie`, defined
//! per branch at a non-empty path `p`:
//! - `hash_mask` bit `i`: child `i` is a branch node;
//! - `tree_mask` bit `i`: the topmost branch of child `i`'s subtree (the child itself, or the
//!   target of an extension child) is stored;
//! - the branch is stored when either mask is non-empty, with the hashes of its `hash_mask`
//!   children in nibble order. The root (empty path) is never stored.

use std::collections::BTreeMap;

use alloy_primitives::{B256, keccak256};
use alloy_rlp::Header;
use alloy_trie::{BranchNodeCompact, EMPTY_ROOT_HASH, Nibbles, TrieMask};

/// Number of nibbles in a hashed key.
pub const KEY_NIBBLES: usize = 64;

/// The reference specification (see the module docs).
#[derive(Debug, Clone, Copy)]
pub struct Spec;

/// One built node: its RLP, and what its parent's masks need to know.
struct Built {
    rlp: Vec<u8>,
    /// The node is a branch.
    is_branch: bool,
    /// The topmost branch of this subtree is stored.
    top_stored: bool,
}

impl Spec {
    /// The hex-prefix encoding of `nibbles` (Yellow Paper appendix C), flagged as a leaf or an
    /// extension.
    pub fn hex_prefix(nibbles: &[u8], leaf: bool) -> Vec<u8> {
        let flag = if leaf { 2u8 } else { 0u8 };
        let odd = nibbles.len() % 2;
        let mut out = Vec::with_capacity(nibbles.len() / 2 + 1);
        let rest = if odd == 1 {
            out.push(((flag + 1) << 4) | nibbles[0]);
            &nibbles[1..]
        } else {
            out.push(flag << 4);
            nibbles
        };
        for pair in rest.chunks(2) {
            out.push((pair[0] << 4) | pair[1]);
        }
        out
    }

    /// RLP of a byte string.
    fn rlp_string(bytes: &[u8], out: &mut Vec<u8>) {
        if bytes.len() == 1 && bytes[0] < 0x80 {
            out.push(bytes[0]);
        } else {
            Header { list: false, payload_length: bytes.len() }.encode(out);
            out.extend_from_slice(bytes);
        }
    }

    /// RLP of a list whose items are already encoded.
    fn rlp_list(items: &[Vec<u8>]) -> Vec<u8> {
        let payload: usize = items.iter().map(Vec::len).sum();
        let mut out = Vec::with_capacity(payload + 9);
        Header { list: true, payload_length: payload }.encode(&mut out);
        for item in items {
            out.extend_from_slice(item);
        }
        out
    }

    /// How a parent refers to a child node: the node itself when shorter than 32 bytes, else
    /// the RLP string of its keccak.
    fn reference(rlp: &[u8]) -> Vec<u8> {
        if rlp.len() < 32 {
            return rlp.to_vec();
        }
        let mut out = Vec::with_capacity(33);
        Self::rlp_string(keccak256(rlp).as_slice(), &mut out);
        out
    }

    /// Builds the node of `keys` (all sharing their first `depth` nibbles), recording stored
    /// branches at their paths in `stored`.
    fn build(
        keys: &[(Vec<u8>, &[u8])],
        depth: usize,
        stored: &mut BTreeMap<Nibbles, BranchNodeCompact>,
    ) -> Built {
        assert!(!keys.is_empty());
        assert!(depth < KEY_NIBBLES);
        if keys.len() == 1 {
            let (key, value) = &keys[0];
            let mut path = Vec::new();
            Self::rlp_string(&Self::hex_prefix(&key[depth..], true), &mut path);
            let mut val = Vec::new();
            Self::rlp_string(value, &mut val);
            return Built {
                rlp: Self::rlp_list(&[path, val]),
                is_branch: false,
                top_stored: false,
            };
        }
        // Keys are sorted, so the first and last share the prefix all keys share.
        let (first, last) = (&keys[0].0, &keys[keys.len() - 1].0);
        let common = (depth..KEY_NIBBLES).take_while(|&i| first[i] == last[i]).count();
        assert!(depth + common < KEY_NIBBLES, "keys are distinct");
        if common > 0 {
            let child = Self::build(keys, depth + common, stored);
            assert!(child.is_branch);
            let mut path = Vec::new();
            Self::rlp_string(&Self::hex_prefix(&first[depth..depth + common], false), &mut path);
            let rlp = Self::rlp_list(&[path, Self::reference(&child.rlp)]);
            return Built { rlp, is_branch: false, top_stored: child.top_stored };
        }
        let (mut state_mask, mut tree_mask, mut hash_mask) =
            (TrieMask::default(), TrieMask::default(), TrieMask::default());
        let mut hashes = Vec::new();
        let mut items = Vec::with_capacity(17);
        let mut start = 0;
        for nibble in 0..16u8 {
            let end = start + keys[start..].iter().take_while(|(k, _)| k[depth] == nibble).count();
            if start == end {
                items.push(vec![0x80]);
                continue;
            }
            let child = Self::build(&keys[start..end], depth + 1, stored);
            state_mask.set_bit(nibble);
            if child.is_branch {
                hash_mask.set_bit(nibble);
                hashes.push(keccak256(&child.rlp));
            }
            if child.top_stored {
                tree_mask.set_bit(nibble);
            }
            items.push(Self::reference(&child.rlp));
            start = end;
        }
        assert_eq!(start, keys.len());
        items.push(vec![0x80]);
        let top_stored = !tree_mask.is_empty() || !hash_mask.is_empty();
        if top_stored && depth > 0 {
            let path = Nibbles::from_nibbles(&keys[0].0[..depth]);
            stored.insert(
                path,
                BranchNodeCompact::new(state_mask, tree_mask, hash_mask, hashes, None),
            );
        }
        Built { rlp: Self::rlp_list(&items), is_branch: true, top_stored }
    }

    /// The root and the stored branch nodes of the trie holding `leaves` (hashed key to the
    /// already encoded leaf value).
    pub fn trie(leaves: &BTreeMap<B256, Vec<u8>>) -> (B256, BTreeMap<Nibbles, BranchNodeCompact>) {
        let mut stored = BTreeMap::new();
        if leaves.is_empty() {
            return (EMPTY_ROOT_HASH, stored);
        }
        let keys: Vec<(Vec<u8>, &[u8])> =
            leaves.iter().map(|(k, v)| (Nibbles::unpack(*k).to_vec(), v.as_slice())).collect();
        let root = Self::build(&keys, 0, &mut stored);
        (keccak256(&root.rlp), stored)
    }

    /// The root of the trie holding `leaves`.
    pub fn root(leaves: &BTreeMap<B256, Vec<u8>>) -> B256 {
        Self::trie(leaves).0
    }
}

#[cfg(test)]
mod tests {
    use alloy_trie::HashBuilder;

    use super::*;

    /// alloy-trie's `HashBuilder` over `leaves`: the root, and the branch nodes it stores (root
    /// path excluded, as reth stores them).
    fn hash_builder(
        leaves: &BTreeMap<B256, Vec<u8>>,
    ) -> (B256, BTreeMap<Nibbles, BranchNodeCompact>) {
        let mut builder = HashBuilder::default().with_updates(true);
        for (key, value) in leaves {
            builder.add_leaf(Nibbles::unpack(*key), value);
        }
        let root = builder.root();
        let (_, nodes) = builder.split();
        (root, nodes.into_iter().filter(|(p, _)| !p.is_empty()).collect())
    }

    /// The key whose nibbles are zero except `digits` at `positions`.
    fn key_at(positions: &[usize], digits: &[u8]) -> B256 {
        let mut nibbles = [0u8; KEY_NIBBLES];
        for (p, d) in positions.iter().zip(digits) {
            nibbles[*p] = *d;
        }
        B256::from_slice(&Nibbles::from_nibbles(nibbles).pack())
    }

    /// Every subset of the keys that vary only at `positions` over `alphabet`, with values of
    /// several lengths (short ones embed leaves in their parent), against the hash builder.
    fn exhaustive(positions: &[usize], alphabet: &[u8]) {
        let mut keys = Vec::new();
        let count = alphabet.len().pow(positions.len() as u32);
        for mut index in 0..count {
            let digits: Vec<u8> = (0..positions.len())
                .map(|_| {
                    let d = alphabet[index % alphabet.len()];
                    index /= alphabet.len();
                    d
                })
                .collect();
            keys.push(key_at(positions, &digits));
        }
        assert!(keys.len() <= 16);
        for subset in 0u32..1 << keys.len() {
            let leaves: BTreeMap<B256, Vec<u8>> = keys
                .iter()
                .enumerate()
                .filter(|(i, _)| subset & (1 << i) != 0)
                .map(|(i, k)| (*k, vec![0x42; 1 + (i * 7 + subset as usize) % 40]))
                .collect();
            assert_eq!(
                Spec::trie(&leaves),
                hash_builder(&leaves),
                "positions {positions:?} subset {subset:#x}"
            );
        }
    }

    #[test]
    fn every_small_trie_matches_the_hash_builder() {
        exhaustive(&[0, 1, 2, 3], &[0, 1]);
        exhaustive(&[0, 5, 6, 40], &[0, 15]);
        exhaustive(&[3, 4], &[0, 7, 9, 15]);
        exhaustive(&[0, 1], &[0, 1, 2, 3]);
        exhaustive(&[1, 30, 31, 59], &[0, 1]);
    }

    #[test]
    fn random_tries_match_the_hash_builder() {
        let mut state = 7u64;
        let mut next = move || {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            state >> 11
        };
        for round in 0..300 {
            let size = next() % if round % 10 == 0 { 3000 } else { 200 };
            let leaves: BTreeMap<B256, Vec<u8>> = (0..size)
                .map(|_| {
                    let mut key = keccak256(next().to_be_bytes()).0;
                    // Shared prefixes make extensions and deep branches.
                    key[0] = (next() % 3) as u8;
                    if next() % 2 == 0 {
                        key[1] = 0x11;
                        key[2] = (next() % 2) as u8;
                    }
                    (B256::from(key), vec![1; 1 + (next() % 70) as usize])
                })
                .collect();
            assert_eq!(Spec::trie(&leaves), hash_builder(&leaves), "round {round}");
        }
    }
}

#[cfg(kani)]
mod proofs {
    use super::*;

    /// Hex-prefix encoding is injective and records parity and kind in the first nibble: decoding
    /// the first byte's flags and the packed nibbles gives back exactly the input, for every
    /// nibble string up to 6 long.
    #[kani::proof]
    #[kani::unwind(8)]
    fn hex_prefix_round_trips() {
        let len: usize = kani::any();
        kani::assume(len <= 6);
        let mut nibbles = [0u8; 6];
        for n in &mut nibbles {
            *n = kani::any();
            kani::assume(*n < 16);
        }
        let leaf: bool = kani::any();
        let out = Spec::hex_prefix(&nibbles[..len], leaf);
        assert_eq!(out.len(), len / 2 + 1);
        let flag = out[0] >> 4;
        assert_eq!(flag & 2 != 0, leaf);
        assert_eq!((flag & 1) as usize, len % 2);
        assert!(flag < 4);
        let mut decoded = [0u8; 6];
        let mut n = 0;
        if len % 2 == 1 {
            decoded[0] = out[0] & 0x0f;
            n = 1;
        } else {
            assert_eq!(out[0] & 0x0f, 0);
        }
        for byte in &out[1..] {
            decoded[n] = byte >> 4;
            decoded[n + 1] = byte & 0x0f;
            n += 2;
        }
        assert_eq!(n, len);
        assert_eq!(&decoded[..len], &nibbles[..len]);
    }
}
