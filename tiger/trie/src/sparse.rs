//! A sparse Merkle Patricia trie over RLP nodes loaded lazily from a [`NodeSource`]: a stateless
//! witness (nodes by keccak hash) or the database (nodes by owner and nibble path).
//!
//! Only the nodes an operation walks are loaded; every other subtree stays a hash. Reads,
//! updates and proofs that need a node the source does not hold fail with
//! [`TrieError::MissingNode`], naming the owner, path and hash. After updates, [`SparseTrie::root`]
//! hashes the changed nodes, and [`SparseTrie::changes`] lists the nodes to persist by path (every
//! node referenced by hash, plus the root) and the stored paths that no longer hold a node.
//!
//! Keys are 32-byte hashed keys (64 nibbles), so a leaf never sits above another leaf and
//! branches never carry values. Walks are loops; only decoding a node's embedded (shorter than
//! 32 bytes) children recurses, at most a few levels.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use alloy_primitives::{B256, Bytes, keccak256};
use alloy_rlp::Decodable;
use alloy_trie::{
    EMPTY_ROOT_HASH, Nibbles, TrieMask,
    nodes::{BranchNodeRef, ExtensionNodeRef, LeafNodeRef, RlpNode, TrieNode},
};

use crate::TrieError;

/// Where trie nodes come from.
pub trait NodeSource {
    /// The RLP of the node at `path` of trie `owner` (`None` for the account trie) whose hash is
    /// `hash`, if this source holds it. The trie checks the hash.
    fn node(&self, owner: Option<B256>, path: &Nibbles, hash: B256) -> Result<Option<Vec<u8>>, TrieError>;

    /// Whether nodes from this source are already persisted (no write needed unless changed).
    fn persisted(&self) -> bool {
        false
    }
}

/// A stateless witness: node RLP by keccak hash.
#[derive(Debug, Default, Clone)]
pub struct WitnessSource {
    /// Nodes by hash.
    pub nodes: HashMap<B256, Bytes>,
}

impl WitnessSource {
    /// Indexes `nodes` by their hash.
    pub fn new(nodes: impl IntoIterator<Item = Bytes>) -> Self {
        Self { nodes: nodes.into_iter().map(|n| (keccak256(&n), n)).collect() }
    }
}

impl NodeSource for WitnessSource {
    fn node(&self, _owner: Option<B256>, _path: &Nibbles, hash: B256) -> Result<Option<Vec<u8>>, TrieError> {
        Ok(self.nodes.get(&hash).map(|n| n.to_vec()))
    }
}

/// A source that holds nothing (a trie built from scratch).
#[derive(Debug, Default, Clone, Copy)]
pub struct NoNodes;

impl NodeSource for NoNodes {
    fn node(&self, _owner: Option<B256>, _path: &Nibbles, _hash: B256) -> Result<Option<Vec<u8>>, TrieError> {
        Ok(None)
    }
}

/// A child reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ref {
    Empty,
    /// A subtree not loaded.
    Hash(B256),
    /// A loaded node in the arena.
    Node(usize),
}

#[derive(Debug, Clone)]
enum Node {
    Leaf { key: Nibbles, value: Vec<u8> },
    Ext { key: Nibbles, child: Ref },
    Branch { children: Box<[Ref; 16]> },
}

impl Node {
    /// The leaf or extension with `prefix` put before its key.
    fn prefixed(self, prefix: &Nibbles) -> Self {
        match self {
            Self::Leaf { key, value } => Self::Leaf { key: prefix.join(&key), value },
            Self::Ext { key, child } => Self::Ext { key: prefix.join(&key), child },
            Self::Branch { .. } => unreachable!("only a leaf or an extension merges a prefix"),
        }
    }
}

#[derive(Debug, Clone)]
struct Entry {
    node: Node,
    /// The node's reference (`rlp(node)` when short, else its hash word); `None` while changed.
    rlp: Option<RlpNode>,
    /// The node's full RLP, when known.
    encoded: Option<Vec<u8>>,
    /// Loaded from a persisted source and unchanged since.
    persisted: bool,
}

/// Where a reference lives: the root, or slot `0..16` of a branch / `16` of an extension.
#[derive(Debug, Clone, Copy)]
enum Loc {
    Root,
    Child(usize, u8),
}

/// A sparse trie over one root.
#[derive(Debug, Clone)]
pub struct SparseTrie {
    owner: Option<B256>,
    root: Ref,
    arena: Vec<Entry>,
    /// Paths of separately stored nodes loaded from a persisted source.
    loaded: BTreeSet<Nibbles>,
}

/// Nodes to persist after updates.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SparseChanges {
    /// Node RLP by path for every node that is new, changed, or came from a non-persisted source.
    pub writes: BTreeMap<Nibbles, Vec<u8>>,
    /// Stored paths that no longer hold a node.
    pub deletes: BTreeSet<Nibbles>,
}

/// Decoding nests at most this deep (embedded nodes are under 32 bytes).
const MAX_EMBED_DEPTH: usize = 8;

impl SparseTrie {
    /// A trie with root `root` for `owner` (`None`: the account trie).
    pub fn new(owner: Option<B256>, root: B256) -> Self {
        let root = if root == EMPTY_ROOT_HASH { Ref::Empty } else { Ref::Hash(root) };
        Self { owner, root, arena: Vec::new(), loaded: BTreeSet::new() }
    }

    /// The trie's owner.
    pub const fn owner(&self) -> Option<B256> {
        self.owner
    }

    fn get_ref(&self, loc: Loc) -> Ref {
        match loc {
            Loc::Root => self.root,
            Loc::Child(i, slot) => match &self.arena[i].node {
                Node::Branch { children } => children[slot as usize],
                Node::Ext { child, .. } => {
                    assert_eq!(slot, 16);
                    *child
                }
                Node::Leaf { .. } => unreachable!("leaves have no children"),
            },
        }
    }

    fn set_ref(&mut self, loc: Loc, r: Ref) {
        match loc {
            Loc::Root => self.root = r,
            Loc::Child(i, slot) => match &mut self.arena[i].node {
                Node::Branch { children } => children[slot as usize] = r,
                Node::Ext { child, .. } => *child = r,
                Node::Leaf { .. } => unreachable!("leaves have no children"),
            },
        }
    }

    fn push(&mut self, node: Node) -> usize {
        self.arena.push(Entry { node, rlp: None, encoded: None, persisted: false });
        self.arena.len() - 1
    }

    /// Decodes `rlp` into the arena (embedded children too).
    fn decode(&mut self, rlp: &[u8], persisted: bool, depth: usize) -> Result<usize, TrieError> {
        assert!(depth < MAX_EMBED_DEPTH, "embedded nodes nest too deep");
        let bad = |e: alloy_rlp::Error| TrieError::Corrupt(format!("bad trie node: {e}"));
        let node = TrieNode::decode(&mut &rlp[..]).map_err(bad)?;
        let child = |this: &mut Self, rlp: &RlpNode| -> Result<Ref, TrieError> {
            Ok(match rlp.as_hash() {
                Some(hash) => Ref::Hash(hash),
                None => Ref::Node(this.decode(rlp.as_slice(), persisted, depth + 1)?),
            })
        };
        let node = match node {
            TrieNode::EmptyRoot => return Err(TrieError::Corrupt("empty node below the root".into())),
            TrieNode::Leaf(leaf) => Node::Leaf { key: leaf.key, value: leaf.value },
            TrieNode::Extension(ext) => {
                if ext.key.is_empty() {
                    return Err(TrieError::Corrupt("extension with an empty key".into()));
                }
                Node::Ext { key: ext.key, child: child(self, &ext.child)? }
            }
            TrieNode::Branch(branch) => {
                let mut children = [Ref::Empty; 16];
                let mut stack = branch.stack.iter();
                for (nibble, slot) in children.iter_mut().enumerate() {
                    if branch.state_mask.is_bit_set(nibble as u8) {
                        *slot = child(self, stack.next().expect("one stack item per set bit"))?;
                    }
                }
                Node::Branch { children: Box::new(children) }
            }
        };
        self.arena.push(Entry { node, rlp: Some(RlpNode::from_rlp(rlp)), encoded: Some(rlp.to_vec()), persisted });
        Ok(self.arena.len() - 1)
    }

    /// The node at `loc` (path `path`), loading it if it is a hash. `None` when empty.
    fn resolve(&mut self, source: &dyn NodeSource, loc: Loc, path: &Nibbles) -> Result<Option<usize>, TrieError> {
        match self.get_ref(loc) {
            Ref::Empty => Ok(None),
            Ref::Node(i) => Ok(Some(i)),
            Ref::Hash(hash) => {
                let missing = || TrieError::MissingNode { owner: self.owner, path: *path, hash };
                let rlp = source.node(self.owner, path, hash)?.ok_or_else(missing)?;
                if keccak256(&rlp) != hash {
                    return Err(TrieError::Corrupt(format!("node at {path:?} does not hash to {hash}")));
                }
                let persisted = source.persisted();
                let i = self.decode(&rlp, persisted, 0)?;
                if persisted {
                    self.loaded.insert(*path);
                }
                self.set_ref(loc, Ref::Node(i));
                Ok(Some(i))
            }
        }
    }

    /// Marks nodes changed so their hashes are recomputed.
    fn dirty(&mut self, nodes: &[usize]) {
        for &i in nodes {
            self.arena[i].rlp = None;
            self.arena[i].encoded = None;
            self.arena[i].persisted = false;
        }
    }

    /// The value at `key`, `None` when absent.
    pub fn get(&mut self, source: &dyn NodeSource, key: B256) -> Result<Option<Vec<u8>>, TrieError> {
        self.walk(source, key, None)
    }

    /// Walks `key`'s path to its leaf or to the node that shows it absent, returning the value
    /// and appending to `proof` every stored node passed (see [`Self::proof`]).
    fn walk(&mut self, source: &dyn NodeSource, key: B256, mut proof: Option<&mut Vec<Bytes>>) -> Result<Option<Vec<u8>>, TrieError> {
        let k = Nibbles::unpack(key);
        let (mut loc, mut depth) = (Loc::Root, 0usize);
        loop {
            let Some(i) = self.resolve(source, loc, &k.slice(..depth))? else { return Ok(None) };
            if let Some(out) = proof.as_deref_mut() {
                if self.arena[i].rlp.is_none() {
                    self.root();
                }
                let entry = &self.arena[i];
                if matches!(loc, Loc::Root) || entry.rlp.as_ref().is_some_and(RlpNode::is_hash) {
                    out.push(Bytes::from(entry.encoded.clone().expect("encoded")));
                }
            }
            match &self.arena[i].node {
                Node::Leaf { key, value } => return Ok((*key == k.slice(depth..)).then(|| value.clone())),
                Node::Ext { key, .. } => {
                    if !k.slice(depth..).starts_with(key) {
                        return Ok(None);
                    }
                    depth += key.len();
                    loc = Loc::Child(i, 16);
                }
                Node::Branch { .. } => {
                    loc = Loc::Child(i, k.get_unchecked(depth));
                    depth += 1;
                }
            }
        }
    }

    /// Sets `key` to `value`, or removes it with `None`.
    pub fn update(&mut self, source: &dyn NodeSource, key: B256, value: Option<Vec<u8>>) -> Result<(), TrieError> {
        let k = Nibbles::unpack(key);
        // Visited nodes with their location and depth, root first.
        let mut chain: Vec<(Loc, usize, usize)> = Vec::with_capacity(16);
        let (mut loc, mut depth) = (Loc::Root, 0usize);
        loop {
            let Some(i) = self.resolve(source, loc, &k.slice(..depth))? else {
                if let Some(value) = value {
                    let leaf = self.push(Node::Leaf { key: k.slice(depth..), value });
                    self.set_ref(loc, Ref::Node(leaf));
                    self.dirty(&chain.iter().map(|c| c.1).collect::<Vec<_>>());
                }
                return Ok(());
            };
            chain.push((loc, i, depth));
            let rest = k.slice(depth..);
            match self.arena[i].node.clone() {
                Node::Leaf { key: leaf_key, value: leaf_value } => {
                    if leaf_key == rest {
                        match value {
                            Some(value) => {
                                self.arena[i].node = Node::Leaf { key: leaf_key, value };
                                self.dirty(&chain.iter().map(|c| c.1).collect::<Vec<_>>());
                            }
                            None => self.remove_leaf(source, &k, chain)?,
                        }
                        return Ok(());
                    }
                    let Some(value) = value else { return Ok(()) };
                    let c = leaf_key.common_prefix_length(&rest);
                    assert!(c < rest.len(), "keys have equal length");
                    let old = self.push(Node::Leaf { key: leaf_key.slice(c + 1..), value: leaf_value });
                    let new = self.push(Node::Leaf { key: rest.slice(c + 1..), value });
                    self.split(i, &rest, c, (leaf_key.get_unchecked(c), Ref::Node(old)), new);
                    self.dirty(&chain.iter().map(|c| c.1).collect::<Vec<_>>());
                    return Ok(());
                }
                Node::Ext { key: ext_key, child } => {
                    if rest.starts_with(&ext_key) {
                        depth += ext_key.len();
                        loc = Loc::Child(i, 16);
                        continue;
                    }
                    let Some(value) = value else { return Ok(()) };
                    let c = ext_key.common_prefix_length(&rest);
                    let below = if c + 1 == ext_key.len() {
                        child
                    } else {
                        Ref::Node(self.push(Node::Ext { key: ext_key.slice(c + 1..), child }))
                    };
                    let new = self.push(Node::Leaf { key: rest.slice(c + 1..), value });
                    self.split(i, &rest, c, (ext_key.get_unchecked(c), below), new);
                    self.dirty(&chain.iter().map(|c| c.1).collect::<Vec<_>>());
                    return Ok(());
                }
                Node::Branch { .. } => {
                    loc = Loc::Child(i, k.get_unchecked(depth));
                    depth += 1;
                }
            }
        }
    }

    /// Replaces node `i` by a branch holding `old` and the new leaf `new`, behind an extension of
    /// the `c` shared nibbles when there are any.
    fn split(&mut self, i: usize, rest: &Nibbles, c: usize, old: (u8, Ref), new: usize) {
        let mut children = [Ref::Empty; 16];
        children[old.0 as usize] = old.1;
        children[rest.get_unchecked(c) as usize] = Ref::Node(new);
        if c == 0 {
            self.arena[i].node = Node::Branch { children: Box::new(children) };
        } else {
            let branch = self.push(Node::Branch { children: Box::new(children) });
            self.arena[i].node = Node::Ext { key: rest.slice(..c), child: Ref::Node(branch) };
        }
    }

    /// Removes the leaf at the end of `chain`, collapsing a branch left with one child.
    fn remove_leaf(&mut self, source: &dyn NodeSource, k: &Nibbles, mut chain: Vec<(Loc, usize, usize)>) -> Result<(), TrieError> {
        let (leaf_loc, _, _) = chain.pop().expect("the leaf");
        self.set_ref(leaf_loc, Ref::Empty);
        let Some(&(_, parent, parent_depth)) = chain.last() else { return Ok(()) };
        let Node::Branch { children } = &self.arena[parent].node else {
            return Err(TrieError::Corrupt("a leaf below an extension".into()));
        };
        let remaining: Vec<u8> = (0..16u8).filter(|n| children[*n as usize] != Ref::Empty).collect();
        assert!(!remaining.is_empty(), "a branch had at least two children");
        self.dirty(&chain.iter().map(|c| c.1).collect::<Vec<_>>());
        if remaining.len() > 1 {
            return Ok(());
        }
        // Collapse the branch into its one remaining child.
        let n = remaining[0];
        let mut child_path = k.slice(..parent_depth);
        child_path.push(n);
        let child = self.resolve(source, Loc::Child(parent, n), &child_path)?.expect("non-empty child");
        let prefix = Nibbles::from_nibbles([n]);
        self.arena[parent].node = match self.arena[child].node.clone() {
            Node::Branch { .. } => Node::Ext { key: prefix, child: Ref::Node(child) },
            node => node.prefixed(&prefix),
        };
        // Merge into an extension above.
        if chain.len() >= 2 {
            let grand = chain[chain.len() - 2].1;
            if let Node::Ext { key: grand_key, .. } = self.arena[grand].node.clone() {
                self.arena[grand].node = self.arena[parent].node.clone().prefixed(&grand_key);
            }
        }
        Ok(())
    }

    /// Hashes changed nodes and returns the root hash.
    pub fn root(&mut self) -> B256 {
        let Ref::Node(root) = self.root else {
            return match self.root {
                Ref::Hash(hash) => hash,
                _ => EMPTY_ROOT_HASH,
            };
        };
        // Post-order over changed nodes.
        let mut stack: Vec<(usize, bool)> = vec![(root, false)];
        let mut buf = Vec::with_capacity(600);
        while let Some((i, expanded)) = stack.pop() {
            if self.arena[i].rlp.is_some() {
                continue;
            }
            if !expanded {
                stack.push((i, true));
                let children: Vec<usize> = match &self.arena[i].node {
                    Node::Leaf { .. } => Vec::new(),
                    Node::Ext { child, .. } => match child {
                        Ref::Node(c) => vec![*c],
                        _ => Vec::new(),
                    },
                    Node::Branch { children } => {
                        children.iter().filter_map(|c| if let Ref::Node(c) = c { Some(*c) } else { None }).collect()
                    }
                };
                stack.extend(children.into_iter().filter(|c| self.arena[*c].rlp.is_none()).map(|c| (c, false)));
                continue;
            }
            buf.clear();
            let child_rlp = |arena: &[Entry], r: &Ref| -> RlpNode {
                match r {
                    Ref::Hash(h) => RlpNode::word_rlp(h),
                    Ref::Node(c) => arena[*c].rlp.clone().expect("children hashed first"),
                    Ref::Empty => unreachable!("empty references are skipped"),
                }
            };
            let rlp = match &self.arena[i].node {
                Node::Leaf { key, value } => LeafNodeRef::new(key, value).rlp(&mut buf),
                Node::Ext { key, child } => {
                    let child = child_rlp(&self.arena, child);
                    ExtensionNodeRef::new(key, child.as_slice()).rlp(&mut buf)
                }
                Node::Branch { children } => {
                    let mut mask = TrieMask::default();
                    let mut stack_items = Vec::with_capacity(16);
                    for (n, c) in children.iter().enumerate() {
                        if *c != Ref::Empty {
                            mask.set_bit(n as u8);
                            stack_items.push(child_rlp(&self.arena, c));
                        }
                    }
                    BranchNodeRef::new(&stack_items, mask).rlp(&mut buf)
                }
            };
            self.arena[i].rlp = Some(rlp);
            self.arena[i].encoded = Some(buf.clone());
        }
        let entry = &self.arena[root];
        let rlp = entry.rlp.as_ref().expect("hashed");
        rlp.as_hash().unwrap_or_else(|| keccak256(entry.encoded.as_ref().expect("encoded")))
    }

    /// The nodes to persist after [`Self::root`]: every separately stored node (the root, and
    /// nodes referenced by hash) that is new or changed or came from a non-persisted source, and
    /// the stored paths that no longer hold a node.
    pub fn changes(&self) -> SparseChanges {
        let mut out = SparseChanges::default();
        let mut live = BTreeSet::new();
        let mut stack: Vec<(Ref, Nibbles, bool)> = vec![(self.root, Nibbles::new(), true)];
        while let Some((r, path, is_root)) = stack.pop() {
            let Ref::Node(i) = r else { continue };
            let entry = &self.arena[i];
            let rlp = entry.rlp.as_ref().expect("call root() first");
            if is_root || rlp.is_hash() {
                live.insert(path);
                if !entry.persisted {
                    out.writes.insert(path, entry.encoded.clone().expect("encoded"));
                }
            }
            match &entry.node {
                Node::Leaf { .. } => {}
                Node::Ext { key, child } => stack.push((*child, path.join(key), false)),
                Node::Branch { children } => {
                    for (n, c) in children.iter().enumerate() {
                        let mut p = path;
                        p.push(n as u8);
                        stack.push((*c, p, false));
                    }
                }
            }
        }
        out.deletes = self.loaded.difference(&live).copied().collect();
        out
    }

    /// The proof of `key` after [`Self::root`]: every stored node from the root along the key's
    /// path (the root first, embedded nodes skipped), ending at the leaf or at the node that shows
    /// the key is absent. Empty for an empty trie.
    pub fn proof(&mut self, source: &dyn NodeSource, key: B256) -> Result<Vec<Bytes>, TrieError> {
        let mut out = Vec::new();
        self.walk(source, key, Some(&mut out))?;
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use alloy_trie::{HashBuilder, proof::verify_proof};

    use super::*;

    /// Nodes by path, as the database keeps them.
    #[derive(Default)]
    struct PathStore(BTreeMap<Nibbles, Vec<u8>>);

    impl NodeSource for PathStore {
        fn node(&self, _owner: Option<B256>, path: &Nibbles, _hash: B256) -> Result<Option<Vec<u8>>, TrieError> {
            Ok(self.0.get(path).cloned())
        }
        fn persisted(&self) -> bool {
            true
        }
    }

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            z ^ (z >> 31)
        }
        /// Keys sharing long prefixes, so tries get extensions, collapses and embedded nodes.
        fn key(&mut self) -> B256 {
            let mut key = [0u8; 32];
            key[0] = (self.next() % 3) as u8;
            // Up to 31 shared bytes: deep leaves are short enough to embed in their parent.
            let shared = (self.next() % 64) as usize;
            let tail = keccak256(self.next().to_be_bytes());
            for i in shared / 2..32 {
                key[i] = tail[i];
            }
            B256::from(key)
        }
    }

    fn scratch_root(model: &BTreeMap<B256, Vec<u8>>) -> B256 {
        let mut hb = HashBuilder::default();
        for (k, v) in model {
            hb.add_leaf(Nibbles::unpack(*k), v);
        }
        hb.root()
    }

    fn stored_from_scratch(model: &BTreeMap<B256, Vec<u8>>) -> BTreeMap<Nibbles, Vec<u8>> {
        let mut trie = SparseTrie::new(None, EMPTY_ROOT_HASH);
        for (k, v) in model {
            trie.update(&NoNodes, *k, Some(v.clone())).unwrap();
        }
        trie.root();
        trie.changes().writes
    }

    #[test]
    fn randomized_updates_match_hash_builder_and_persisted_nodes() {
        for seed in 1..=20u64 {
            let mut rng = Rng(seed);
            let mut model: BTreeMap<B256, Vec<u8>> = BTreeMap::new();
            let mut store = PathStore::default();
            let mut root = EMPTY_ROOT_HASH;
            let mut keys: Vec<B256> = Vec::new();
            for round in 0..40 {
                let mut trie = SparseTrie::new(None, root);
                for _ in 0..1 + rng.next() % 20 {
                    let delete = !keys.is_empty() && rng.next().is_multiple_of(3);
                    if delete {
                        let key = keys[(rng.next() % keys.len() as u64) as usize];
                        model.remove(&key);
                        trie.update(&store, key, None).unwrap();
                    } else {
                        let key = if !keys.is_empty() && rng.next().is_multiple_of(4) {
                            keys[(rng.next() % keys.len() as u64) as usize]
                        } else {
                            let k = rng.key();
                            keys.push(k);
                            k
                        };
                        // Small values make embedded nodes likely.
                        let value = vec![1 + (rng.next() % 200) as u8; 1 + (rng.next() % 3) as usize];
                        model.insert(key, value.clone());
                        trie.update(&store, key, Some(value)).unwrap();
                    }
                }
                root = trie.root();
                assert_eq!(root, scratch_root(&model), "seed {seed} round {round}");
                let changes = trie.changes();
                for path in &changes.deletes {
                    store.0.remove(path);
                }
                store.0.extend(changes.writes);
                if model.is_empty() {
                    store.0.clear();
                }
                assert_eq!(store.0, stored_from_scratch(&model), "seed {seed} round {round}: stored nodes");
                // Reads and proofs through the stored nodes.
                let mut reader = SparseTrie::new(None, root);
                for _ in 0..4 {
                    let key = if keys.is_empty() || rng.next().is_multiple_of(3) { rng.key() } else { keys[(rng.next() % keys.len() as u64) as usize] };
                    assert_eq!(reader.get(&store, key).unwrap(), model.get(&key).cloned());
                    let proof = reader.proof(&store, key).unwrap();
                    verify_proof(root, Nibbles::unpack(key), model.get(&key).cloned(), &proof).unwrap();
                }
            }
        }
    }

    #[test]
    fn witness_holds_only_the_touched_paths() {
        let mut rng = Rng(99);
        let mut model = BTreeMap::new();
        let mut full = SparseTrie::new(None, EMPTY_ROOT_HASH);
        for _ in 0..300 {
            let key = rng.key();
            let value = vec![7u8; 40];
            model.insert(key, value.clone());
            full.update(&NoNodes, key, Some(value)).unwrap();
        }
        let root = full.root();
        let keys: Vec<B256> = model.keys().copied().collect();
        // A witness for reading and deleting keys[3] and writing a new key.
        let new_key = rng.key();
        let mut nodes = full.proof(&NoNodes, keys[3]).unwrap();
        nodes.extend(full.proof(&NoNodes, new_key).unwrap());
        // Deleting may collapse a branch into its sibling, which the witness must hold too.
        let mut probe = SparseTrie::new(None, root);
        let all: BTreeMap<B256, Bytes> =
            full.changes().writes.into_values().map(|n| (keccak256(&n), Bytes::from(n))).collect();
        let everything = WitnessSource { nodes: all.into_iter().collect() };
        probe.update(&everything, keys[3], None).unwrap();
        let mut witness = WitnessSource::new(nodes);
        // Add every node the delete loaded, the collapse sibling among them.
        for entry in &probe.arena {
            if let Some(encoded) = &entry.encoded
                && encoded.len() >= 32
            {
                witness.nodes.insert(keccak256(encoded), Bytes::from(encoded.clone()));
            }
        }
        let mut sparse = SparseTrie::new(None, root);
        assert_eq!(sparse.get(&witness, keys[3]).unwrap(), model.get(&keys[3]).cloned());
        sparse.update(&witness, keys[3], None).unwrap();
        sparse.update(&witness, new_key, Some(vec![9u8; 40])).unwrap();
        model.remove(&keys[3]);
        model.insert(new_key, vec![9u8; 40]);
        assert_eq!(sparse.root(), scratch_root(&model));
        // A key far from the touched paths is not in the witness.
        let untouched = keys.iter().find(|k| k.0[..2] != keys[3].0[..2] && k.0[..2] != new_key.0[..2]).unwrap();
        let mut fresh = SparseTrie::new(None, root);
        match fresh.get(&WitnessSource::new(full.proof(&NoNodes, keys[3]).unwrap()), *untouched) {
            Err(TrieError::MissingNode { owner: None, path, .. }) => assert!(!path.is_empty()),
            other => panic!("expected a missing node, got {other:?}"),
        }
    }
}
