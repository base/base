//! Merkle-Patricia trie construction that keeps every node referenced by hash.

use alloy_primitives::{B256, Bytes, keccak256};
use alloy_rlp::{BufMut, Encodable, Header};
use alloy_trie::EMPTY_ROOT_HASH;

/// Builds a full Merkle-Patricia trie from its leaves.
///
/// Returns the root hash and the RLP of every node stored by hash. Nodes whose encoding is
/// shorter than 32 bytes are inlined in their parent and are not stored separately; the root is
/// always stored because the header references it by hash.
#[derive(Debug, Default)]
pub struct TrieBuilder {
    /// Every node stored by hash, in construction order.
    pub nodes: Vec<Bytes>,
}

/// How a parent refers to a child node.
enum ChildRef {
    /// The child's own RLP, embedded because it is shorter than 32 bytes.
    Inline(Vec<u8>),
    /// The keccak hash of the child's RLP.
    Hash(B256),
}

impl TrieBuilder {
    /// Builds the trie for `(hashed key, RLP value)` leaves, collecting nodes into `self.nodes`.
    ///
    /// Leaves may be given in any order; keys must be distinct.
    pub fn build(&mut self, mut leaves: Vec<(B256, Vec<u8>)>) -> B256 {
        if leaves.is_empty() {
            return EMPTY_ROOT_HASH;
        }
        leaves.sort_by_key(|a| a.0);
        let keyed: Vec<([u8; 64], &[u8])> =
            leaves.iter().map(|(key, value)| (Self::nibbles(key), value.as_slice())).collect();
        let root = self.node(&keyed, 0);
        let rlp = match root {
            ChildRef::Inline(rlp) => rlp,
            ChildRef::Hash(hash) => return hash,
        };
        let hash = keccak256(&rlp);
        self.nodes.push(rlp.into());
        hash
    }

    fn nibbles(key: &B256) -> [u8; 64] {
        let mut out = [0u8; 64];
        for (i, byte) in key.iter().enumerate() {
            out[2 * i] = byte >> 4;
            out[2 * i + 1] = byte & 0x0f;
        }
        out
    }

    fn node(&mut self, leaves: &[([u8; 64], &[u8])], depth: usize) -> ChildRef {
        if leaves.len() == 1 {
            let (key, value) = leaves[0];
            let mut payload = Vec::new();
            Self::hex_prefix(&key[depth..], true).as_slice().encode(&mut payload);
            value.encode(&mut payload);
            return self.finish(payload);
        }
        let first = &leaves[0].0;
        let last = &leaves[leaves.len() - 1].0;
        let mut common = 0;
        while depth + common < 64 && first[depth + common] == last[depth + common] {
            common += 1;
        }
        if common > 0 {
            let child = self.branch(leaves, depth + common);
            let mut payload = Vec::new();
            Self::hex_prefix(&first[depth..depth + common], false).as_slice().encode(&mut payload);
            Self::put_child(&mut payload, Some(child));
            return self.finish(payload);
        }
        self.branch(leaves, depth)
    }

    fn branch(&mut self, leaves: &[([u8; 64], &[u8])], depth: usize) -> ChildRef {
        let mut payload = Vec::new();
        let mut start = 0;
        for nibble in 0..16u8 {
            let end =
                start + leaves[start..].iter().take_while(|(key, _)| key[depth] == nibble).count();
            let child = (end > start).then(|| self.node(&leaves[start..end], depth + 1));
            Self::put_child(&mut payload, child);
            start = end;
        }
        // Branch value slot: keys have equal length, so branches never hold values.
        payload.put_u8(alloy_rlp::EMPTY_STRING_CODE);
        self.finish(payload)
    }

    fn put_child(payload: &mut Vec<u8>, child: Option<ChildRef>) {
        match child {
            None => payload.put_u8(alloy_rlp::EMPTY_STRING_CODE),
            Some(ChildRef::Inline(rlp)) => payload.extend_from_slice(&rlp),
            Some(ChildRef::Hash(hash)) => hash.encode(payload),
        }
    }

    fn finish(&mut self, payload: Vec<u8>) -> ChildRef {
        let mut rlp = Vec::with_capacity(payload.len() + 3);
        Header { list: true, payload_length: payload.len() }.encode(&mut rlp);
        rlp.extend_from_slice(&payload);
        if rlp.len() < 32 {
            return ChildRef::Inline(rlp);
        }
        let hash = keccak256(&rlp);
        self.nodes.push(rlp.into());
        ChildRef::Hash(hash)
    }

    fn hex_prefix(path: &[u8], leaf: bool) -> Vec<u8> {
        let flag = if leaf { 2 } else { 0 };
        let mut out = Vec::with_capacity(path.len() / 2 + 1);
        let rest = if path.len() % 2 == 1 {
            out.push(((flag + 1) << 4) | path[0]);
            &path[1..]
        } else {
            out.push(flag << 4);
            path
        };
        for pair in rest.chunks(2) {
            out.push((pair[0] << 4) | pair[1]);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use alloy_primitives::{B256, U256, keccak256};
    use alloy_rlp::Encodable;
    use alloy_trie::{EMPTY_ROOT_HASH, HashBuilder, Nibbles};

    use super::TrieBuilder;
    use crate::r#gen::rng::Rng;

    fn reference_root(leaves: &[(B256, Vec<u8>)]) -> B256 {
        let mut sorted = leaves.to_vec();
        sorted.sort_by_key(|a| a.0);
        let mut builder = HashBuilder::default();
        for (key, value) in &sorted {
            builder.add_leaf(Nibbles::unpack(key), value);
        }
        builder.root()
    }

    /// Every hash reference inside every node, found by scanning for `0xa0 || 32 bytes`.
    fn referenced_hashes(node: &[u8]) -> Vec<B256> {
        let items: Vec<alloy_rlp::Bytes> = alloy_rlp::decode_exact(node).unwrap_or_default();
        items.iter().filter(|item| item.len() == 32).map(|item| B256::from_slice(item)).collect()
    }

    #[test]
    fn roots_match_alloy_trie() {
        let mut rng = Rng::new(&[7]);
        assert_eq!(TrieBuilder::default().build(Vec::new()), EMPTY_ROOT_HASH);
        for size in [1usize, 2, 3, 5, 16, 17, 100, 600] {
            for value_len in [1usize, 3, 33, 80] {
                let leaves: Vec<(B256, Vec<u8>)> = (0..size)
                    .map(|i| {
                        let mut value = Vec::new();
                        rng.bytes(value_len).as_slice().encode(&mut value);
                        (keccak256(U256::from(i).to_be_bytes::<32>()), value)
                    })
                    .collect();
                let mut builder = TrieBuilder::default();
                let root = builder.build(leaves.clone());
                assert_eq!(root, reference_root(&leaves), "size {size} value {value_len}");

                // The node set is closed: the root and every hash reference resolve to a node.
                let hashes: HashSet<B256> = builder.nodes.iter().map(keccak256).collect();
                assert!(hashes.contains(&root));
                for node in &builder.nodes {
                    assert!(node.len() >= 32 || keccak256(node) == root);
                    for child in referenced_hashes(node) {
                        // Leaf values may themselves be 32-byte strings; only branch and
                        // extension children are node references.
                        if items_is_leaf(node) {
                            continue;
                        }
                        assert!(hashes.contains(&child), "dangling child {child}");
                    }
                }
            }
        }
    }

    fn items_is_leaf(node: &[u8]) -> bool {
        let items: Vec<alloy_rlp::Bytes> = alloy_rlp::decode_exact(node).unwrap_or_default();
        items.len() == 2 && items[0].first().is_some_and(|b| b >> 4 >= 2)
    }

    #[test]
    fn short_storage_values_inline_small_tries() {
        // Two one-byte storage values make leaves short enough to inline under the root branch.
        let leaves: Vec<(B256, Vec<u8>)> = (0..2u64)
            .map(|i| {
                let mut value = Vec::new();
                U256::from(1).encode(&mut value);
                (keccak256(U256::from(i).to_be_bytes::<32>()), value)
            })
            .collect();
        let mut builder = TrieBuilder::default();
        let root = builder.build(leaves.clone());
        assert_eq!(root, reference_root(&leaves));
        assert!(!builder.nodes.is_empty());
    }
}
