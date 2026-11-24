//! Merkle Patricia trie range proofs, as snap uses them: a run of consecutive leaves plus the
//! trie nodes on the paths to the range's first and last key prove that no leaf in between was
//! left out or changed.
//!
//! Verification rebuilds the root. The proof nodes are walked from the root; subtrees entirely
//! left or right of the range keep their hash commitment, subtrees entirely inside are replaced
//! by the supplied leaves, and boundary nodes are expanded. Feeding the commitments and leaves in
//! key order to a hash builder must reproduce the requested root. This is the algorithm reth's
//! `verify_range_proof` uses (`crates/trie/common/src/range_proof.rs`), written over alloy-trie.

use alloy_primitives::{B256, Bytes, keccak256};
use alloy_rlp::Decodable;
use alloy_trie::{
    HashBuilder, Nibbles,
    nodes::{RlpNode, TrieNode},
};
use std::collections::HashMap;

const KEY_NIBBLES: usize = 64;

/// Why a range proof is rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RangeProofError {
    /// Leaves are not strictly increasing, start before the origin, or have empty values.
    BadLeaves,
    /// A node on a boundary path is missing from the proof.
    MissingNode,
    /// A node is malformed or its path exceeds a hashed key.
    BadNode,
    /// The rebuilt root differs from the expected root.
    RootMismatch,
}

/// The outcome of a verified range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RangeProofOutcome {
    /// `true` if the trie has keys after the proven range (more to fetch).
    pub more: bool,
}

enum Entry {
    Leaf(Nibbles, Vec<u8>),
    Subtree(Nibbles, B256),
}

impl Entry {
    const fn path(&self) -> &Nibbles {
        match self {
            Self::Leaf(path, _) | Self::Subtree(path, _) => path,
        }
    }
}

struct Verifier<'a> {
    left: Nibbles,
    right: Nibbles,
    nodes: HashMap<B256, &'a [u8]>,
    entries: Vec<Entry>,
    more: bool,
}

impl Verifier<'_> {
    fn visit_reference(
        &mut self,
        prefix: Nibbles,
        reference: &RlpNode,
    ) -> Result<(), RangeProofError> {
        if prefix.len() > KEY_NIBBLES {
            return Err(RangeProofError::BadNode);
        }
        let left = self.left.slice(..prefix.len());
        let right = self.right.slice(..prefix.len());
        if prefix < left {
            self.add_outside(prefix, reference)
        } else if prefix > right {
            self.more = true;
            self.add_outside(prefix, reference)
        } else if prefix > left && prefix < right {
            Ok(()) // covered by the supplied leaves
        } else {
            let node = self.resolve(reference)?;
            self.visit_node(node, prefix)
        }
    }

    fn resolve(&self, reference: &RlpNode) -> Result<TrieNode, RangeProofError> {
        match reference.as_hash() {
            Some(hash) => {
                let bytes = self.nodes.get(&hash).ok_or(RangeProofError::MissingNode)?;
                Ok(TrieNode::decode(&mut &bytes[..])?)
            }
            None => Ok(TrieNode::decode(&mut reference.as_slice())?),
        }
    }

    fn visit_node(&mut self, node: TrieNode, prefix: Nibbles) -> Result<(), RangeProofError> {
        match node {
            TrieNode::EmptyRoot => Ok(()),
            TrieNode::Leaf(leaf) => {
                let path = join(&prefix, &leaf.key, true)?;
                if path < self.left {
                    self.entries.push(Entry::Leaf(path, leaf.value));
                } else if path > self.right {
                    self.more = true;
                    self.entries.push(Entry::Leaf(path, leaf.value));
                }
                Ok(())
            }
            TrieNode::Extension(extension) => {
                if extension.key.is_empty() {
                    return Err(RangeProofError::BadNode);
                }
                let path = join(&prefix, &extension.key, false)?;
                self.visit_reference(path, &extension.child)
            }
            TrieNode::Branch(branch) => {
                for (nibble, child) in branch.as_ref().children() {
                    if let Some(child) = child {
                        let mut path = prefix;
                        if path.len() >= KEY_NIBBLES {
                            return Err(RangeProofError::BadNode);
                        }
                        path.push(nibble);
                        self.visit_reference(path, child)?;
                    }
                }
                Ok(())
            }
        }
    }

    fn add_outside(&mut self, prefix: Nibbles, reference: &RlpNode) -> Result<(), RangeProofError> {
        if let Some(hash) = reference.as_hash() {
            self.entries.push(Entry::Subtree(prefix, hash));
            return Ok(());
        }
        // An inline node (shorter than 32 bytes): descend to leaves and hashed children.
        match TrieNode::decode(&mut reference.as_slice()).map_err(|_| RangeProofError::BadNode)? {
            TrieNode::EmptyRoot => Ok(()),
            TrieNode::Leaf(leaf) => {
                let path = join(&prefix, &leaf.key, true)?;
                self.entries.push(Entry::Leaf(path, leaf.value));
                Ok(())
            }
            TrieNode::Extension(extension) => {
                if extension.key.is_empty() {
                    return Err(RangeProofError::BadNode);
                }
                let path = join(&prefix, &extension.key, false)?;
                self.add_outside(path, &extension.child)
            }
            TrieNode::Branch(branch) => {
                for (nibble, child) in branch.as_ref().children() {
                    if let Some(child) = child {
                        let mut path = prefix;
                        if path.len() >= KEY_NIBBLES {
                            return Err(RangeProofError::BadNode);
                        }
                        path.push(nibble);
                        self.add_outside(path, child)?;
                    }
                }
                Ok(())
            }
        }
    }
}

impl From<alloy_rlp::Error> for RangeProofError {
    fn from(_: alloy_rlp::Error) -> Self {
        Self::BadNode
    }
}

fn join(prefix: &Nibbles, key: &Nibbles, leaf: bool) -> Result<Nibbles, RangeProofError> {
    if prefix.len() + key.len() > KEY_NIBBLES {
        return Err(RangeProofError::BadNode);
    }
    let path = prefix.join(key);
    if leaf && path.len() != KEY_NIBBLES {
        return Err(RangeProofError::BadNode);
    }
    Ok(path)
}

fn rebuild(mut entries: Vec<Entry>) -> Result<B256, RangeProofError> {
    entries.sort_by(|a, b| a.path().cmp(b.path()));
    let mut builder = HashBuilder::default();
    let mut previous: Option<Nibbles> = None;
    for entry in entries {
        let path = *entry.path();
        if previous.is_some_and(|p| path <= p) {
            return Err(RangeProofError::BadNode);
        }
        previous = Some(path);
        match entry {
            Entry::Leaf(path, value) => builder.add_leaf(path, &value),
            Entry::Subtree(path, hash) => builder.add_branch(path, hash, false),
        }
    }
    Ok(builder.root())
}

/// Verifies that `leaves` (hashed key, RLP value) are exactly the trie's leaves from `origin` up
/// to the last leaf (or up to `limit` if `leaves` is empty), given the boundary `proof`.
///
/// An empty `proof` is only valid when `leaves` is the whole trie.
pub fn verify_range_proof(
    root: B256,
    origin: B256,
    limit: B256,
    leaves: &[(B256, Vec<u8>)],
    proof: &[Bytes],
) -> Result<RangeProofOutcome, RangeProofError> {
    let mut entries = Vec::with_capacity(leaves.len() + 32);
    let mut previous: Option<B256> = None;
    for (key, value) in leaves {
        if *key < origin || previous.is_some_and(|p| *key <= p) || value.is_empty() {
            return Err(RangeProofError::BadLeaves);
        }
        previous = Some(*key);
        entries.push(Entry::Leaf(Nibbles::unpack(key), value.clone()));
    }
    if proof.is_empty() {
        if rebuild(entries)? != root {
            return Err(RangeProofError::RootMismatch);
        }
        return Ok(RangeProofOutcome { more: false });
    }
    let right = previous.unwrap_or(limit);
    let mut verifier = Verifier {
        left: Nibbles::unpack(origin),
        right: Nibbles::unpack(right),
        nodes: proof.iter().map(|node| (keccak256(node), node.as_ref())).collect(),
        entries,
        more: false,
    };
    verifier.visit_reference(Nibbles::default(), &RlpNode::word_rlp(&root))?;
    let more = verifier.more;
    if rebuild(verifier.entries)? != root {
        return Err(RangeProofError::RootMismatch);
    }
    Ok(RangeProofOutcome { more })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_trie::proof::ProofRetainer;

    /// Builds a trie over `leaves`, returning its root and the proof nodes for `targets`.
    fn trie(leaves: &[(B256, Vec<u8>)], targets: &[B256]) -> (B256, Vec<Bytes>) {
        let retainer = ProofRetainer::new(targets.iter().map(Nibbles::unpack).collect());
        let mut builder = HashBuilder::default().with_proof_retainer(retainer);
        for (key, value) in leaves {
            builder.add_leaf(Nibbles::unpack(key), value);
        }
        let root = builder.root();
        let proof =
            builder.take_proof_nodes().into_nodes_sorted().into_iter().map(|(_, n)| n).collect();
        (root, proof)
    }

    fn leaves(n: u64) -> Vec<(B256, Vec<u8>)> {
        let mut out: Vec<_> =
            (0..n).map(|i| (keccak256(i.to_be_bytes()), alloy_rlp::encode(i + 1))).collect();
        out.sort();
        out
    }

    #[test]
    fn accepts_whole_trie_without_proof() {
        let all = leaves(50);
        let (root, _) = trie(&all, &[]);
        let outcome =
            verify_range_proof(root, B256::ZERO, B256::repeat_byte(0xff), &all, &[]).unwrap();
        assert!(!outcome.more);
        assert_eq!(
            verify_range_proof(root, B256::ZERO, B256::ZERO, &all[1..], &[]),
            Err(RangeProofError::RootMismatch)
        );
    }

    #[test]
    fn verifies_middle_ranges_and_rejects_tampering() {
        let all = leaves(300);
        for (from, to) in [(0usize, 10usize), (17, 120), (250, 300), (5, 6)] {
            let origin = all[from].0;
            let last = all[to - 1].0;
            let (root, proof) = trie(&all, &[origin, last]);
            let range = &all[from..to];
            let outcome = verify_range_proof(root, origin, last, range, &proof).unwrap();
            assert_eq!(outcome.more, to < all.len(), "range {from}..{to}");

            // A missing leaf in the middle fails.
            if range.len() > 2 {
                let mut gap = range.to_vec();
                gap.remove(1);
                assert!(verify_range_proof(root, origin, last, &gap, &proof).is_err());
            }
            // A changed value fails.
            let mut changed = range.to_vec();
            changed[0].1 = alloy_rlp::encode(999_999u64);
            assert!(verify_range_proof(root, origin, last, &changed, &proof).is_err());
            // Dropping the last leaf fails: its proof path no longer matches the boundary.
            if range.len() > 1 {
                assert!(
                    verify_range_proof(root, origin, last, &range[..range.len() - 1], &proof)
                        .is_err()
                );
            }
        }
    }

    #[test]
    fn proves_an_empty_range_and_an_origin_between_keys() {
        let all = leaves(100);
        // Origin strictly between two keys: prove the origin path and the last leaf.
        let mut origin = all[40].0;
        origin.0[31] = origin.0[31].wrapping_add(1);
        assert!(origin < all[41].0);
        let last = all[60].0;
        let (root, proof) = trie(&all, &[origin, last]);
        let outcome = verify_range_proof(root, origin, last, &all[41..61], &proof).unwrap();
        assert!(outcome.more);
        // Omitting the first leaf after the origin is caught.
        assert!(verify_range_proof(root, origin, last, &all[42..61], &proof).is_err());

        // An empty range past the last key proves there is nothing more.
        let mut after = all[99].0;
        after.0[31] = after.0[31].wrapping_add(1);
        let (root, proof) = trie(&all, &[after]);
        let outcome =
            verify_range_proof(root, after, B256::repeat_byte(0xff), &[], &proof).unwrap();
        assert!(!outcome.more);
    }
}
