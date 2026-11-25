//! A Merkle Patricia trie kept in memory across blocks, revealed lazily from the stored trie.
//!
//! [`LiveTrie`] holds the parts of one trie (the account trie, or one account's storage trie)
//! that blocks touched, as decoded nodes with their hashes. Every other subtree is a blinded
//! branch: its hash and whether reth stores it. Updates reveal what they walk through from a
//! [`LiveSource`] (the stored branch rows and the flat leaves, as the walker reads them), then
//! [`LiveTrie::root`] hashes only the changed nodes and lists the stored branch rows that change,
//! with the same masks as alloy-trie's `HashBuilder` and reth's walker give them (see
//! [`crate::Spec`] for the definition).
//!
//! The trie is split into 16 shards, one per first nibble, each with its own node arena, so
//! shards reveal, update and hash on separate threads. The root node (above the shards) is never
//! stored, so the root is recombined from the shard tops on every call.
//!
//! Keys are 64-nibble hashed keys: a leaf never sits above another leaf, and branches carry no
//! value. Revealed subtrees stay valid as long as the trie is only changed through this type in
//! the same order the stored trie is: the caller keeps it in step with one chain of blocks.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    sync::Mutex,
};

use alloy_primitives::{B256, keccak256};
use alloy_rlp::Encodable;
use alloy_trie::{
    BranchNodeCompact, EMPTY_ROOT_HASH, Nibbles, TrieMask,
    nodes::{BranchNodeRef, ExtensionNodeRef, LeafNodeRef, RlpNode},
};

use crate::{
    HashedState, HashedStorage, KEY_NIBBLES, StorageTrieUpdates, TrieError, TrieUpdates,
    account_leaf, storage_leaf,
};

/// Where a [`LiveTrie`] reveals nodes from: the stored branch rows and flat leaves of the state
/// the trie currently represents.
pub trait LiveSource {
    /// The stored branch row at exactly `path` of trie `owner` (`None`: the account trie).
    fn branch(
        &mut self,
        owner: Option<B256>,
        path: &Nibbles,
    ) -> Result<Option<BranchNodeCompact>, TrieError>;

    /// The first stored branch row, in path order, whose path starts with `prefix`.
    fn first_branch_under(
        &mut self,
        owner: Option<B256>,
        prefix: &Nibbles,
    ) -> Result<Option<(Nibbles, BranchNodeCompact)>, TrieError>;

    /// Appends every leaf whose key starts with `prefix` to `out`, in key order: the hashed key
    /// and the encoded leaf value.
    fn leaves(
        &mut self,
        owner: Option<B256>,
        prefix: &Nibbles,
        out: &mut Vec<(B256, Vec<u8>)>,
    ) -> Result<(), TrieError>;
}

/// No node: the reference of an empty child.
const EMPTY: u32 = u32::MAX;

#[derive(Debug, Clone)]
enum Node {
    /// A branch not revealed: its reference is the entry's `rlp`, and its storedness the entry's
    /// `top_stored`.
    Blind,
    /// A leaf with the rest of its key.
    Leaf { key: Nibbles, value: Vec<u8> },
    /// An extension.
    Ext { key: Nibbles, child: u32 },
    /// A branch. `stored`: reth holds a row for it in the stored trie this trie moves away from.
    Branch { children: [u32; 16], stored: bool },
    /// A free arena slot.
    Free,
}

#[derive(Debug, Clone)]
struct Entry {
    node: Node,
    /// How the parent refers to this node; `None` until (re)hashed.
    rlp: Option<RlpNode>,
    /// Changed since the stored trie (a changed branch writes or deletes its row when hashed).
    changed: bool,
    /// The topmost branch of this subtree is stored. Valid while `rlp` is set.
    top_stored: bool,
}

/// One first-nibble subtree.
#[derive(Debug, Clone, Default)]
struct Shard {
    /// The node at the shard's one-nibble path; `None` until revealed.
    top: Option<u32>,
    arena: Vec<Entry>,
    free: Vec<u32>,
}

/// Stored branch row changes by path; `None` deletes the row.
pub type NodeUpdates = BTreeMap<Nibbles, Option<BranchNodeCompact>>;

/// One key's change: its new encoded leaf from its current one (`None`: absent); `None` removes it.
pub type LeafChange<'a> = dyn FnMut(Option<&[u8]>) -> Result<Option<Vec<u8>>, TrieError> + 'a;

/// Account leaves before a stream of changes first changed them, by hashed address.
pub type Originals = Mutex<HashMap<B256, Option<Vec<u8>>>>;

/// Opens a [`LiveSource`] for one thread.
pub type SourceFn<'a> = dyn Fn() -> Result<Box<dyn LiveSource + 'a>, TrieError> + Sync + 'a;

/// A leaf change for [`LiveTrie::update_all`]: the new encoded value of the key from its current
/// one (`None`: absent); `None` removes it.
pub type LeafFn<'a> =
    dyn Fn(&B256, Option<&[u8]>) -> Result<Option<Vec<u8>>, TrieError> + Sync + 'a;

/// Where a computation reads: one source on the calling thread, or one source per worker thread.
pub enum Sources<'s, 'a> {
    /// The calling thread, with this source.
    Local(&'s mut (dyn LiveSource + 'a)),
    /// Up to this many threads, each opening its own source.
    Threads(&'s SourceFn<'a>, usize),
}

impl std::fmt::Debug for Sources<'_, '_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Local(_) => f.write_str("Sources::Local"),
            Self::Threads(_, threads) => write!(f, "Sources::Threads({threads})"),
        }
    }
}

/// Fewer changed keys than this update a trie on the calling thread.
const PARALLEL_MIN_KEYS: usize = 64;

/// Where a reference sits: the shard top, or child `0..16` of a branch / the child of an
/// extension (16).
#[derive(Debug, Clone, Copy)]
enum Loc {
    Top,
    Child(u32, u8),
}

impl Shard {
    fn alloc(&mut self, node: Node, rlp: Option<RlpNode>, top_stored: bool) -> u32 {
        let entry = Entry { node, rlp, changed: false, top_stored };
        if let Some(i) = self.free.pop() {
            self.arena[i as usize] = entry;
            return i;
        }
        assert!(self.arena.len() < EMPTY as usize, "arena full");
        self.arena.push(entry);
        (self.arena.len() - 1) as u32
    }

    fn release(&mut self, i: u32) {
        self.arena[i as usize] =
            Entry { node: Node::Free, rlp: None, changed: false, top_stored: false };
        self.free.push(i);
    }

    fn get(&self, loc: Loc) -> u32 {
        match loc {
            Loc::Top => self.top.expect("revealed"),
            Loc::Child(i, slot) => match &self.arena[i as usize].node {
                Node::Branch { children, .. } => children[slot as usize],
                Node::Ext { child, .. } => *child,
                other => unreachable!("{other:?} has no children"),
            },
        }
    }

    fn set(&mut self, loc: Loc, r: u32) {
        match loc {
            Loc::Top => self.top = Some(r),
            Loc::Child(i, slot) => match &mut self.arena[i as usize].node {
                Node::Branch { children, .. } => children[slot as usize] = r,
                Node::Ext { child, .. } => *child = r,
                other => unreachable!("{other:?} has no children"),
            },
        }
    }

    /// Builds the subtree of every key under `prefix` (at least one nibble long) from `source`.
    fn build(
        &mut self,
        source: &mut dyn LiveSource,
        owner: Option<B256>,
        prefix: &Nibbles,
    ) -> Result<u32, TrieError> {
        assert!(!prefix.is_empty());
        if let Some((path, row)) = source.first_branch_under(owner, prefix)? {
            // Every branch with a stored descendant is stored, so the first stored row under
            // `prefix` is the subtree's topmost branch, below an extension when deeper.
            assert!(path.starts_with(prefix) && path.len() < KEY_NIBBLES);
            let branch = self.build_row(source, owner, &path, &row)?;
            if path.len() == prefix.len() {
                return Ok(branch);
            }
            return Ok(self.alloc(
                Node::Ext { key: path.slice(prefix.len()..), child: branch },
                None,
                false,
            ));
        }
        let mut leaves = Vec::new();
        source.leaves(owner, prefix, &mut leaves)?;
        if leaves.is_empty() {
            return Ok(EMPTY);
        }
        let keys: Vec<(Nibbles, Vec<u8>)> =
            leaves.into_iter().map(|(k, v)| (Nibbles::unpack(k), v)).collect();
        assert!(keys.iter().all(|(k, _)| k.starts_with(prefix)));
        Ok(self.build_leaves(&keys, prefix.len()))
    }

    /// The branch at `path` from its stored row: hashed children stay blinded, the others (leaves,
    /// and extensions) are built.
    fn build_row(
        &mut self,
        source: &mut dyn LiveSource,
        owner: Option<B256>,
        path: &Nibbles,
        row: &BranchNodeCompact,
    ) -> Result<u32, TrieError> {
        let corrupt = || {
            TrieError::Corrupt(format!(
                "stored branch at {path:?} of {owner:?} disagrees with the leaves"
            ))
        };
        let mut children = [EMPTY; 16];
        let mut hashes = row.hashes.iter();
        for nibble in 0..16u8 {
            if !row.state_mask.is_bit_set(nibble) {
                continue;
            }
            let mut child_path = *path;
            child_path.push(nibble);
            children[nibble as usize] = if row.hash_mask.is_bit_set(nibble) {
                let hash = hashes.next().ok_or_else(corrupt)?;
                self.alloc(
                    Node::Blind,
                    Some(RlpNode::word_rlp(hash)),
                    row.tree_mask.is_bit_set(nibble),
                )
            } else {
                let child = self.build(source, owner, &child_path)?;
                if child == EMPTY || matches!(self.arena[child as usize].node, Node::Branch { .. })
                {
                    return Err(corrupt());
                }
                child
            };
        }
        Ok(self.alloc(Node::Branch { children, stored: true }, None, true))
    }

    /// The subtree of `keys` (sorted, sharing their first `depth` nibbles), none of it stored.
    fn build_leaves(&mut self, keys: &[(Nibbles, Vec<u8>)], depth: usize) -> u32 {
        assert!(!keys.is_empty());
        if keys.len() == 1 {
            let (key, value) = &keys[0];
            return self.alloc(
                Node::Leaf { key: key.slice(depth..), value: value.clone() },
                None,
                false,
            );
        }
        let common =
            keys[0].0.slice(depth..).common_prefix_length(&keys[keys.len() - 1].0.slice(depth..));
        if common > 0 {
            let child = self.build_leaves(keys, depth + common);
            return self.alloc(
                Node::Ext { key: keys[0].0.slice(depth..depth + common), child },
                None,
                false,
            );
        }
        let mut children = [EMPTY; 16];
        let mut start = 0;
        while start < keys.len() {
            let nibble = keys[start].0.get_unchecked(depth);
            let end = start
                + keys[start..]
                    .iter()
                    .take_while(|(k, _)| k.get_unchecked(depth) == nibble)
                    .count();
            children[nibble as usize] = self.build_leaves(&keys[start..end], depth + 1);
            start = end;
        }
        self.alloc(Node::Branch { children, stored: false }, None, false)
    }

    /// Reveals the node at `loc` (path `path`) when it is blinded, in place.
    fn resolve(
        &mut self,
        source: &mut dyn LiveSource,
        owner: Option<B256>,
        loc: Loc,
        path: &Nibbles,
    ) -> Result<u32, TrieError> {
        let i = self.get(loc);
        if i == EMPTY || !matches!(self.arena[i as usize].node, Node::Blind) {
            return Ok(i);
        }
        let stored = self.arena[i as usize].top_stored;
        let revealed = if stored {
            let row = source.branch(owner, path)?.ok_or_else(|| TrieError::MissingNode {
                owner,
                path: *path,
                hash: self.arena[i as usize]
                    .rlp
                    .as_ref()
                    .and_then(RlpNode::as_hash)
                    .unwrap_or_default(),
            })?;
            self.build_row(source, owner, path, &row)?
        } else {
            let mut leaves = Vec::new();
            source.leaves(owner, path, &mut leaves)?;
            let keys: Vec<(Nibbles, Vec<u8>)> =
                leaves.into_iter().map(|(k, v)| (Nibbles::unpack(k), v)).collect();
            if keys.len() < 2
                || keys[0].0.get_unchecked(path.len())
                    == keys[keys.len() - 1].0.get_unchecked(path.len())
            {
                return Err(TrieError::Corrupt(format!(
                    "blinded branch at {path:?} of {owner:?} has no branch below"
                )));
            }
            self.build_leaves(&keys, path.len())
        };
        // Keep the index (the parent refers to it) and the known hash.
        let node = std::mem::replace(&mut self.arena[revealed as usize].node, Node::Free);
        self.release(revealed);
        self.arena[i as usize].node = node;
        Ok(i)
    }

    fn touch(&mut self, chain: &[(Loc, u32, usize)]) {
        for &(_, i, _) in chain {
            let entry = &mut self.arena[i as usize];
            entry.rlp = None;
            entry.changed = true;
        }
    }

    /// Sets `key` (whose first nibble is this shard's) to the value `change` makes of its current
    /// one (`None`: absent), or removes it when `change` gives `None`. Stored branch paths the
    /// change removes are recorded in `removed`.
    fn update(
        &mut self,
        source: &mut dyn LiveSource,
        owner: Option<B256>,
        key: &Nibbles,
        change: &mut LeafChange<'_>,
        removed: &mut NodeUpdates,
    ) -> Result<(), TrieError> {
        if self.top.is_none() {
            let top = self.build(source, owner, &key.slice(..1))?;
            self.top = Some(top);
        }
        // Visited nodes with their location and depth, top first.
        let mut chain: Vec<(Loc, u32, usize)> = Vec::with_capacity(16);
        let (mut loc, mut depth) = (Loc::Top, 1usize);
        loop {
            let i = self.resolve(source, owner, loc, &key.slice(..depth))?;
            if i == EMPTY {
                if let Some(value) = change(None)? {
                    let leaf =
                        self.alloc(Node::Leaf { key: key.slice(depth..), value }, None, false);
                    self.set(loc, leaf);
                    self.touch(&chain);
                }
                return Ok(());
            }
            chain.push((loc, i, depth));
            let rest = key.slice(depth..);
            match &self.arena[i as usize].node {
                Node::Leaf { key: leaf_key, .. } => {
                    let leaf_key = *leaf_key;
                    if leaf_key == rest {
                        let Node::Leaf { value: old, .. } = &self.arena[i as usize].node else {
                            unreachable!()
                        };
                        match change(Some(old))? {
                            // Unchanged: nothing to rehash or rewrite.
                            Some(value) if value == *old => {}
                            Some(value) => {
                                self.arena[i as usize].node = Node::Leaf { key: leaf_key, value };
                                self.touch(&chain);
                            }
                            None => self.remove_leaf(key, chain, removed),
                        }
                        return Ok(());
                    }
                    let Some(value) = change(None)? else { return Ok(()) };
                    let c = leaf_key.common_prefix_length(&rest);
                    assert!(c < rest.len(), "keys have equal length");
                    let Node::Leaf { value: old_value, .. } =
                        std::mem::replace(&mut self.arena[i as usize].node, Node::Free)
                    else {
                        unreachable!()
                    };
                    let old = self.alloc(
                        Node::Leaf { key: leaf_key.slice(c + 1..), value: old_value },
                        None,
                        false,
                    );
                    let new =
                        self.alloc(Node::Leaf { key: rest.slice(c + 1..), value }, None, false);
                    self.split(i, &rest, c, (leaf_key.get_unchecked(c), old), new);
                    self.touch(&chain);
                    return Ok(());
                }
                Node::Ext { key: ext_key, child } => {
                    let (ext_key, child) = (*ext_key, *child);
                    if rest.starts_with(&ext_key) {
                        depth += ext_key.len();
                        loc = Loc::Child(i, 16);
                        continue;
                    }
                    let Some(value) = change(None)? else { return Ok(()) };
                    let c = ext_key.common_prefix_length(&rest);
                    let below = if c + 1 == ext_key.len() {
                        child
                    } else {
                        self.alloc(Node::Ext { key: ext_key.slice(c + 1..), child }, None, false)
                    };
                    let new =
                        self.alloc(Node::Leaf { key: rest.slice(c + 1..), value }, None, false);
                    self.split(i, &rest, c, (ext_key.get_unchecked(c), below), new);
                    self.touch(&chain);
                    return Ok(());
                }
                Node::Branch { .. } => {
                    loc = Loc::Child(i, key.get_unchecked(depth));
                    depth += 1;
                }
                Node::Blind | Node::Free => unreachable!("resolved"),
            }
        }
    }

    /// Replaces node `i` by a branch holding `old` and the new leaf `new`, behind an extension of
    /// the `c` shared nibbles when there are any.
    fn split(&mut self, i: u32, rest: &Nibbles, c: usize, old: (u8, u32), new: u32) {
        let mut children = [EMPTY; 16];
        children[old.0 as usize] = old.1;
        children[rest.get_unchecked(c) as usize] = new;
        if c == 0 {
            self.arena[i as usize].node = Node::Branch { children, stored: false };
        } else {
            let branch = self.alloc(Node::Branch { children, stored: false }, None, false);
            self.arena[branch as usize].changed = true;
            self.arena[i as usize].node = Node::Ext { key: rest.slice(..c), child: branch };
        }
    }

    /// Removes the leaf at the end of `chain`, collapsing a branch left with one child (and
    /// merging it into an extension above).
    fn remove_leaf(
        &mut self,
        key: &Nibbles,
        mut chain: Vec<(Loc, u32, usize)>,
        removed: &mut NodeUpdates,
    ) {
        let (leaf_loc, leaf, _) = chain.pop().expect("the leaf");
        self.set(leaf_loc, EMPTY);
        self.release(leaf);
        self.touch(&chain);
        let Some(&(_, parent, parent_depth)) = chain.last() else { return };
        let Node::Branch { children, stored } = &self.arena[parent as usize].node else {
            unreachable!("a leaf sits below a branch or at the top");
        };
        let mut remaining = (0..16u8).filter(|n| children[*n as usize] != EMPTY);
        let n = remaining.next().expect("a branch had two children");
        if remaining.next().is_some() {
            return;
        }
        // Collapse the branch into its one remaining child.
        if *stored {
            removed.insert(key.slice(..parent_depth), None);
        }
        let child = children[n as usize];
        let prefix = Nibbles::from_nibbles([n]);
        let replacement = match &self.arena[child as usize].node {
            Node::Leaf { key, value } => {
                Some(Node::Leaf { key: prefix.join(key), value: value.clone() })
            }
            Node::Ext { key, child } => Some(Node::Ext { key: prefix.join(key), child: *child }),
            Node::Branch { .. } | Node::Blind => None,
            Node::Free => unreachable!("live child"),
        };
        match replacement {
            Some(node) => {
                self.release(child);
                self.arena[parent as usize].node = node;
            }
            None => self.arena[parent as usize].node = Node::Ext { key: prefix, child },
        }
        // Merge into an extension above.
        if chain.len() >= 2 {
            let grand = chain[chain.len() - 2].1;
            if let Node::Ext { key: grand_key, .. } = &self.arena[grand as usize].node {
                let grand_key = *grand_key;
                let merged =
                    match std::mem::replace(&mut self.arena[parent as usize].node, Node::Free) {
                        Node::Leaf { key, value } => {
                            Node::Leaf { key: grand_key.join(&key), value }
                        }
                        Node::Ext { key, child } => Node::Ext { key: grand_key.join(&key), child },
                        _ => unreachable!("the parent was just collapsed"),
                    };
                self.release(parent);
                self.arena[grand as usize].node = merged;
            }
        }
    }

    /// Hashes every node without a reference below `top`, recording changed branches' rows.
    fn hash(&mut self, top: u32, top_path: Nibbles, updates: &mut NodeUpdates) {
        if top == EMPTY || self.arena[top as usize].rlp.is_some() {
            return;
        }
        // Post-order: (node, path, children pushed).
        let mut stack: Vec<(u32, Nibbles, bool)> = vec![(top, top_path, false)];
        let mut buf = Vec::with_capacity(600);
        let mut items: Vec<RlpNode> = Vec::with_capacity(16);
        while let Some((i, path, expanded)) = stack.pop() {
            if !expanded {
                stack.push((i, path, true));
                match &self.arena[i as usize].node {
                    Node::Ext { key, child } if self.arena[*child as usize].rlp.is_none() => {
                        stack.push((*child, path.join(key), false));
                    }
                    Node::Branch { children, .. } => {
                        for (n, c) in children.iter().enumerate() {
                            if *c != EMPTY && self.arena[*c as usize].rlp.is_none() {
                                let mut p = path;
                                p.push(n as u8);
                                stack.push((*c, p, false));
                            }
                        }
                    }
                    _ => {}
                }
                continue;
            }
            buf.clear();
            let (rlp, top_stored) = match &self.arena[i as usize].node {
                Node::Leaf { key, value } => (LeafNodeRef::new(key, value).rlp(&mut buf), false),
                Node::Ext { key, child } => {
                    let child = &self.arena[*child as usize];
                    let rlp =
                        ExtensionNodeRef::new(key, child.rlp.as_ref().expect("hashed").as_slice())
                            .rlp(&mut buf);
                    (rlp, child.top_stored)
                }
                Node::Branch { children, stored } => {
                    let (mut state_mask, mut tree_mask, mut hash_mask) =
                        (TrieMask::default(), TrieMask::default(), TrieMask::default());
                    let mut hashes = Vec::new();
                    items.clear();
                    for (n, c) in children.iter().enumerate() {
                        if *c == EMPTY {
                            continue;
                        }
                        let child = &self.arena[*c as usize];
                        let rlp = child.rlp.clone().expect("hashed");
                        state_mask.set_bit(n as u8);
                        if matches!(child.node, Node::Branch { .. } | Node::Blind) {
                            hash_mask.set_bit(n as u8);
                            hashes
                                .push(rlp.as_hash().expect("branches encode to 32 bytes or more"));
                        }
                        if child.top_stored {
                            tree_mask.set_bit(n as u8);
                        }
                        items.push(rlp);
                    }
                    let rlp = BranchNodeRef::new(&items, state_mask).rlp(&mut buf);
                    let now_stored = !tree_mask.is_empty() || !hash_mask.is_empty();
                    if self.arena[i as usize].changed {
                        if now_stored {
                            updates.insert(
                                path,
                                Some(BranchNodeCompact::new(
                                    state_mask, tree_mask, hash_mask, hashes, None,
                                )),
                            );
                        } else if *stored {
                            updates.insert(path, None);
                        }
                    }
                    if let Node::Branch { stored, .. } = &mut self.arena[i as usize].node {
                        *stored = now_stored;
                    }
                    (rlp, now_stored)
                }
                Node::Blind | Node::Free => unreachable!("blinded nodes keep their reference"),
            };
            let entry = &mut self.arena[i as usize];
            entry.rlp = Some(rlp);
            entry.top_stored = top_stored;
            entry.changed = false;
        }
    }
}

/// One trie kept in memory (see the module docs).
#[derive(Debug, Clone)]
pub struct LiveTrie {
    owner: Option<B256>,
    shards: Box<[Shard; 16]>,
}

impl LiveTrie {
    /// A trie of `owner` (`None`: the account trie) that reveals everything from its source.
    pub fn new(owner: Option<B256>) -> Self {
        Self { owner, shards: Box::default() }
    }

    /// A trie of `owner` known to be empty (a new or wiped storage), revealing nothing.
    pub fn empty(owner: Option<B256>) -> Self {
        let mut trie = Self::new(owner);
        for shard in trie.shards.iter_mut() {
            shard.top = Some(EMPTY);
        }
        trie
    }

    /// The trie's owner.
    pub const fn owner(&self) -> Option<B256> {
        self.owner
    }

    /// Sets `key` to the encoded leaf `value`, or removes it with `None`. Stored rows the change
    /// removes are recorded in `updates`.
    pub fn update(
        &mut self,
        source: &mut dyn LiveSource,
        key: B256,
        value: Option<Vec<u8>>,
        updates: &mut NodeUpdates,
    ) -> Result<(), TrieError> {
        let mut value = Some(value);
        self.update_with(source, key, &mut |_| Ok(value.take().expect("called once")), updates)
    }

    /// Sets `key` to the encoded leaf `change` makes of its current one (`None`: absent), or
    /// removes it when `change` gives `None`, walking the key's path once.
    pub fn update_with(
        &mut self,
        source: &mut dyn LiveSource,
        key: B256,
        change: &mut LeafChange<'_>,
        updates: &mut NodeUpdates,
    ) -> Result<(), TrieError> {
        let key = Nibbles::unpack(key);
        let owner = self.owner;
        self.shards[key.get_unchecked(0) as usize].update(source, owner, &key, change, updates)
    }

    /// Applies `change` (see [`Self::update_with`]) to every key of `keys` (sorted), then hashes
    /// and returns the root, recording stored row changes in `updates`. Shards run on up to
    /// threads as `sources` allows.
    pub fn update_all(
        &mut self,
        sources: &mut Sources<'_, '_>,
        keys: &[B256],
        change: &LeafFn<'_>,
        updates: &mut NodeUpdates,
    ) -> Result<B256, TrieError> {
        debug_assert!(keys.is_sorted());
        let owner = self.owner;
        let run = |n: usize,
                   shard: &mut Shard,
                   source: &mut dyn LiveSource,
                   out: &mut NodeUpdates|
         -> Result<(), TrieError> {
            let prefix = Nibbles::from_nibbles([n as u8]);
            if shard.top.is_none() {
                shard.top = Some(shard.build(source, owner, &prefix)?);
            }
            let start = keys.partition_point(|k| (k.0[0] >> 4) < n as u8);
            let end = keys.partition_point(|k| (k.0[0] >> 4) <= n as u8);
            for key in &keys[start..end] {
                shard.update(
                    source,
                    owner,
                    &Nibbles::unpack(*key),
                    &mut |old| change(key, old),
                    out,
                )?;
            }
            let top = shard.top.expect("revealed");
            shard.hash(top, prefix, out);
            Ok(())
        };
        let (sources, threads) = match sources {
            Sources::Threads(sources, threads)
                if *threads > 1 && keys.len() >= PARALLEL_MIN_KEYS =>
            {
                (*sources, *threads)
            }
            Sources::Threads(sources, _) => {
                let mut source = sources()?;
                for (n, shard) in self.shards.iter_mut().enumerate() {
                    run(n, shard, &mut *source, updates)?;
                }
                return Ok(self.combine());
            }
            Sources::Local(source) => {
                for (n, shard) in self.shards.iter_mut().enumerate() {
                    run(n, shard, &mut **source, updates)?;
                }
                return Ok(self.combine());
            }
        };
        let next = std::sync::atomic::AtomicUsize::new(0);
        let shards: Vec<std::sync::Mutex<(&mut Shard, NodeUpdates)>> = self
            .shards
            .iter_mut()
            .map(|s| std::sync::Mutex::new((s, NodeUpdates::new())))
            .collect();
        std::thread::scope(|scope| -> Result<(), TrieError> {
            let workers: Vec<_> = (0..threads.min(16))
                .map(|_| {
                    scope.spawn(|| -> Result<(), TrieError> {
                        let mut source = sources()?;
                        loop {
                            let n = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            if n >= 16 {
                                return Ok(());
                            }
                            let mut guard = shards[n].lock().expect("unpoisoned");
                            let (shard, out) = &mut *guard;
                            run(n, shard, &mut *source, out)?;
                        }
                    })
                })
                .collect();
            for worker in workers {
                worker.join().expect("root worker panicked")?;
            }
            Ok(())
        })?;
        for shard in shards {
            updates.extend(shard.into_inner().expect("unpoisoned").1);
        }
        Ok(self.combine())
    }

    /// The encoded leaf at `key`, `None` when absent.
    pub fn get(
        &mut self,
        source: &mut dyn LiveSource,
        key: B256,
    ) -> Result<Option<Vec<u8>>, TrieError> {
        let key = Nibbles::unpack(key);
        let owner = self.owner;
        let shard = &mut self.shards[key.get_unchecked(0) as usize];
        if shard.top.is_none() {
            let top = shard.build(source, owner, &key.slice(..1))?;
            shard.top = Some(top);
        }
        let (mut loc, mut depth) = (Loc::Top, 1usize);
        loop {
            let i = shard.resolve(source, owner, loc, &key.slice(..depth))?;
            if i == EMPTY {
                return Ok(None);
            }
            match &shard.arena[i as usize].node {
                Node::Leaf { key: leaf_key, value } => {
                    return Ok((*leaf_key == key.slice(depth..)).then(|| value.clone()));
                }
                Node::Ext { key: ext_key, .. } => {
                    if !key.slice(depth..).starts_with(ext_key) {
                        return Ok(None);
                    }
                    depth += ext_key.len();
                    loc = Loc::Child(i, 16);
                }
                Node::Branch { .. } => {
                    loc = Loc::Child(i, key.get_unchecked(depth));
                    depth += 1;
                }
                Node::Blind | Node::Free => unreachable!("resolved"),
            }
        }
    }

    /// Reveals the shard tops not revealed yet, which the root needs.
    pub fn reveal_tops(&mut self, source: &mut dyn LiveSource) -> Result<(), TrieError> {
        for (n, shard) in self.shards.iter_mut().enumerate() {
            if shard.top.is_none() {
                let top = shard.build(source, self.owner, &Nibbles::from_nibbles([n as u8]))?;
                shard.top = Some(top);
            }
        }
        Ok(())
    }

    /// Hashes the changed nodes and returns the root, recording the stored rows that change in
    /// `updates`. Reveals the shard tops first.
    pub fn root(
        &mut self,
        source: &mut dyn LiveSource,
        updates: &mut NodeUpdates,
    ) -> Result<B256, TrieError> {
        self.reveal_tops(source)?;
        for (n, shard) in self.shards.iter_mut().enumerate() {
            let top = shard.top.expect("revealed");
            shard.hash(top, Nibbles::from_nibbles([n as u8]), updates);
        }
        Ok(self.combine())
    }

    /// The root from the hashed shard tops.
    fn combine(&self) -> B256 {
        let tops: Vec<(u8, &Shard, u32)> = self
            .shards
            .iter()
            .enumerate()
            .map(|(n, s)| (n as u8, s, s.top.expect("revealed")))
            .filter(|(_, _, top)| *top != EMPTY)
            .collect();
        let mut buf = Vec::with_capacity(600);
        match tops.as_slice() {
            [] => return EMPTY_ROOT_HASH,
            [(n, shard, top)] => {
                // One shard: the root is its top with the shard nibble prepended.
                let prefix = Nibbles::from_nibbles([*n]);
                let entry = &shard.arena[*top as usize];
                match &entry.node {
                    Node::Leaf { key, value } => {
                        LeafNodeRef::new(&prefix.join(key), value).encode(&mut buf)
                    }
                    Node::Ext { key, child } => {
                        let child = shard.arena[*child as usize].rlp.as_ref().expect("hashed");
                        ExtensionNodeRef::new(&prefix.join(key), child.as_slice()).encode(&mut buf)
                    }
                    Node::Branch { .. } | Node::Blind => ExtensionNodeRef::new(
                        &prefix,
                        entry.rlp.as_ref().expect("hashed").as_slice(),
                    )
                    .encode(&mut buf),
                    Node::Free => unreachable!("live top"),
                }
            }
            _ => {
                let mut mask = TrieMask::default();
                let items: Vec<RlpNode> = tops
                    .iter()
                    .map(|(n, shard, top)| {
                        mask.set_bit(*n);
                        shard.arena[*top as usize].rlp.clone().expect("hashed")
                    })
                    .collect();
                BranchNodeRef::new(&items, mask).encode(&mut buf)
            }
        }
        keccak256(&buf)
    }

    /// Asserts the trie's structural invariants after [`Self::root`]: every branch has two or
    /// more children, every extension has a non-empty key and a branch below it, every leaf
    /// ends at the full key length, every change is hashed, and the arena's live entries
    /// are exactly the reachable ones.
    pub fn check(&self) {
        for (n, shard) in self.shards.iter().enumerate() {
            let Some(top) = shard.top else { continue };
            let mut reachable = 0usize;
            let mut stack = vec![(top, 1usize)];
            while let Some((i, depth)) = stack.pop() {
                if i == EMPTY {
                    continue;
                }
                reachable += 1;
                let entry = &shard.arena[i as usize];
                // Children of a revealed but unchanged node may stay unhashed until it changes.
                assert!(
                    !entry.changed && (i != top || entry.rlp.is_some()),
                    "shard {n}: unhashed change at depth {depth}"
                );
                match &entry.node {
                    Node::Blind => assert!(entry.rlp.as_ref().is_some_and(RlpNode::is_hash)),
                    Node::Leaf { key, .. } => {
                        assert_eq!(depth + key.len(), KEY_NIBBLES, "shard {n}: leaf length")
                    }
                    Node::Ext { key, child } => {
                        assert!(
                            !key.is_empty() && depth + key.len() < KEY_NIBBLES,
                            "shard {n}: extension key"
                        );
                        assert!(
                            matches!(
                                shard.arena[*child as usize].node,
                                Node::Branch { .. } | Node::Blind
                            ),
                            "shard {n}: extension above a non-branch"
                        );
                        stack.push((*child, depth + key.len()));
                    }
                    Node::Branch { children, .. } => {
                        assert!(
                            children.iter().filter(|c| **c != EMPTY).count() >= 2,
                            "shard {n}: branch with one child"
                        );
                        stack.extend(children.iter().map(|c| (*c, depth + 1)));
                    }
                    Node::Free => panic!("shard {n}: reachable free entry"),
                }
            }
            assert_eq!(
                reachable,
                shard.arena.len() - shard.free.len(),
                "shard {n}: unreachable live entries"
            );
        }
    }

    /// Nodes held (revealed or blinded), for bounding memory.
    pub fn len(&self) -> usize {
        self.shards.iter().map(|s| s.arena.len() - s.free.len()).sum()
    }

    /// Whether nothing is held.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// The account trie and the storage tries blocks touched, kept in memory (see the module docs).
#[derive(Debug, Clone)]
pub struct LiveState {
    /// The account trie.
    pub accounts: LiveTrie,
    /// Storage tries by hashed address.
    pub storages: HashMap<B256, LiveTrie>,
}

impl Default for LiveState {
    fn default() -> Self {
        Self { accounts: LiveTrie::new(None), storages: HashMap::new() }
    }
}

/// Where the storage root's 32 bytes start, counted back from the end of an encoded account leaf
/// (`rlp([nonce, balance, storage_root, code_hash])`, the code hash last after its `0xa0`).
const STORAGE_ROOT_FROM_END: usize = 65;

impl LiveState {
    /// The storage root in an encoded account leaf.
    fn leaf_storage_root(leaf: &[u8]) -> B256 {
        let n = leaf.len();
        assert!(n > 66 && leaf[n - 66] == 0xa0 && leaf[n - 33] == 0xa0, "an account leaf");
        B256::from_slice(&leaf[n - STORAGE_ROOT_FROM_END..n - STORAGE_ROOT_FROM_END + 32])
    }

    /// Applies `state` (one block's hashed changes over the state this trie holds) and returns
    /// the new state root with the trie updates that move the stored trie to it, as
    /// [`crate::TrieView::state_root`] gives them. The trie then holds the new state.
    ///
    /// Fails with [`TrieError::Unknown`] for changes the walker treats specially and this does
    /// not model (storage changes of an account that does not exist after them). On any error
    /// the trie is left part-way: the caller drops it and uses the walker.
    pub fn apply(
        &mut self,
        sources: &mut Sources<'_, '_>,
        state: &HashedState,
    ) -> Result<(B256, TrieUpdates), TrieError> {
        self.apply_over(sources, state, &BTreeSet::new(), None)
    }

    /// [`Self::apply`] where the storage wipes of `wiped` are already applied to this trie (by
    /// a stream of the block's transactions): their tries are kept, not emptied again. Each
    /// changed account's leaf before the change is recorded in `originals` unless it holds one.
    pub fn apply_over(
        &mut self,
        sources: &mut Sources<'_, '_>,
        state: &HashedState,
        wiped: &BTreeSet<B256>,
        originals: Option<&Originals>,
    ) -> Result<(B256, TrieUpdates), TrieError> {
        let mut updates = TrieUpdates::default();
        // Storage tries first: account leaves need their roots.
        let mut work: Vec<(B256, LiveTrie, &HashedStorage)> = state
            .storages
            .iter()
            .map(|(address, changes)| {
                let trie = match self.storages.remove(address) {
                    Some(trie) if !changes.wiped || wiped.contains(address) => trie,
                    _ if changes.wiped => LiveTrie::empty(Some(*address)),
                    _ => LiveTrie::new(Some(*address)),
                };
                (*address, trie, changes)
            })
            .collect();
        // Largest first, so one big trie does not finish last.
        work.sort_by_key(|(_, _, changes)| std::cmp::Reverse(changes.slots.len()));
        let results = Self::storage_roots(sources, &mut work)?;
        let mut storage_roots: HashMap<B256, B256> = HashMap::with_capacity(work.len());
        for ((address, trie, changes), (root, nodes)) in work.into_iter().zip(results) {
            storage_roots.insert(address, root);
            updates
                .storage_tries
                .insert(address, StorageTrieUpdates { wiped: changes.wiped, nodes });
            self.storages.insert(address, trie);
        }
        for (address, account) in &state.accounts {
            if account.is_some() {
                continue;
            }
            // The walker keeps no storage trie changes for an account that ends absent, except
            // the wipe of its stored nodes.
            if let Some(storage) = updates.storage_tries.get(address)
                && (!storage.wiped || !storage.nodes.is_empty())
            {
                return Err(TrieError::Unknown(format!(
                    "storage changes of deleted account {address}"
                )));
            }
            self.storages.remove(address);
        }
        let changed: Vec<B256> = state
            .accounts
            .keys()
            .chain(state.storages.keys())
            .copied()
            .collect::<BTreeSet<B256>>()
            .into_iter()
            .collect();
        let change = |address: &B256, old: Option<&[u8]>| -> Result<Option<Vec<u8>>, TrieError> {
            if let Some(originals) = originals {
                originals
                    .lock()
                    .expect("unpoisoned")
                    .entry(*address)
                    .or_insert_with(|| old.map(<[u8]>::to_vec));
            }
            Ok(match state.accounts.get(address) {
                Some(None) => None,
                Some(Some(account)) => {
                    let storage_root = match storage_roots.get(address) {
                        Some(root) => *root,
                        None => old.map_or(EMPTY_ROOT_HASH, Self::leaf_storage_root),
                    };
                    Some(account_leaf(account, storage_root))
                }
                None => {
                    // Storage changes only: the account keeps its fields. An absent account
                    // stays absent whatever its storage (the walker has no leaf for it either).
                    let Some(old) = old else {
                        return Ok(None);
                    };
                    let mut leaf = old.to_vec();
                    let n = leaf.len();
                    Self::leaf_storage_root(old);
                    leaf[n - STORAGE_ROOT_FROM_END..n - STORAGE_ROOT_FROM_END + 32]
                        .copy_from_slice(storage_roots[address].as_slice());
                    Some(leaf)
                }
            })
        };
        let root =
            self.accounts.update_all(sources, &changed, &change, &mut updates.account_nodes)?;
        Ok((root, updates))
    }

    /// The new roots and row changes of the storage tries in `work`.
    fn storage_roots(
        sources: &mut Sources<'_, '_>,
        work: &mut [(B256, LiveTrie, &HashedStorage)],
    ) -> Result<Vec<(B256, NodeUpdates)>, TrieError> {
        let one = |source: &mut dyn LiveSource,
                   trie: &mut LiveTrie,
                   changes: &HashedStorage|
         -> Result<(B256, NodeUpdates), TrieError> {
            let mut nodes = NodeUpdates::new();
            for (slot, value) in &changes.slots {
                let leaf = (!value.is_zero()).then(|| storage_leaf(*value));
                trie.update(source, *slot, leaf, &mut nodes)?;
            }
            Ok((trie.root(source, &mut nodes)?, nodes))
        };
        let (sources, threads) = match sources {
            Sources::Threads(sources, threads) if *threads > 1 && work.len() >= 2 => {
                (*sources, *threads)
            }
            Sources::Threads(sources, _) => {
                let mut source = sources()?;
                return work
                    .iter_mut()
                    .map(|(_, trie, changes)| one(&mut *source, trie, changes))
                    .collect();
            }
            Sources::Local(source) => {
                return work
                    .iter_mut()
                    .map(|(_, trie, changes)| one(&mut **source, trie, changes))
                    .collect();
            }
        };
        let next = std::sync::atomic::AtomicUsize::new(0);
        type Cell<'w> = Mutex<(&'w mut LiveTrie, &'w HashedStorage, Option<(B256, NodeUpdates)>)>;
        let cells: Vec<Cell<'_>> = work
            .iter_mut()
            .map(|(_, trie, changes)| Mutex::new((trie, &**changes, None)))
            .collect();
        std::thread::scope(|scope| -> Result<(), TrieError> {
            let workers: Vec<_> = (0..threads.min(cells.len()))
                .map(|_| {
                    scope.spawn(|| -> Result<(), TrieError> {
                        let mut source = sources()?;
                        loop {
                            let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            let Some(cell) = cells.get(i) else { return Ok(()) };
                            let mut guard = cell.lock().expect("unpoisoned");
                            let (trie, changes, out) = &mut *guard;
                            *out = Some(one(&mut *source, trie, changes)?);
                        }
                    })
                })
                .collect();
            for worker in workers {
                worker.join().expect("storage root worker panicked")?;
            }
            Ok(())
        })?;
        Ok(cells
            .into_iter()
            .map(|c| c.into_inner().expect("unpoisoned").2.expect("computed"))
            .collect())
    }

    /// Nodes held across every trie, for bounding memory.
    pub fn len(&self) -> usize {
        self.accounts.len() + self.storages.values().map(LiveTrie::len).sum::<usize>()
    }

    /// Whether nothing is held.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::U256;
    use base_tiger_store::Account;

    use super::*;
    use crate::Spec;

    /// A stored trie in memory: the rows and leaves of one trie, as the datadir holds them.
    #[derive(Debug, Default, Clone)]
    struct MemSource {
        leaves: BTreeMap<B256, Vec<u8>>,
        rows: BTreeMap<Nibbles, BranchNodeCompact>,
    }

    impl MemSource {
        fn first_under(&self, prefix: &Nibbles) -> Option<(Nibbles, BranchNodeCompact)> {
            self.rows
                .range(*prefix..)
                .next()
                .filter(|(p, _)| p.starts_with(prefix))
                .map(|(p, n)| (*p, n.clone()))
        }

        fn leaves_under(&self, prefix: &Nibbles, out: &mut Vec<(B256, Vec<u8>)>) {
            out.extend(
                self.leaves
                    .iter()
                    .filter(|(k, _)| Nibbles::unpack(**k).starts_with(prefix))
                    .map(|(k, v)| (*k, v.clone())),
            );
        }
    }

    impl LiveSource for MemSource {
        fn branch(
            &mut self,
            _owner: Option<B256>,
            path: &Nibbles,
        ) -> Result<Option<BranchNodeCompact>, TrieError> {
            Ok(self.rows.get(path).cloned())
        }

        fn first_branch_under(
            &mut self,
            _owner: Option<B256>,
            prefix: &Nibbles,
        ) -> Result<Option<(Nibbles, BranchNodeCompact)>, TrieError> {
            Ok(self.first_under(prefix))
        }

        fn leaves(
            &mut self,
            _owner: Option<B256>,
            prefix: &Nibbles,
            out: &mut Vec<(B256, Vec<u8>)>,
        ) -> Result<(), TrieError> {
            self.leaves_under(prefix, out);
            Ok(())
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

        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }

        /// Keys clustered under a few prefixes of random length, so tries get extensions, deep
        /// branches, collapses and embedded leaves.
        fn key(&mut self) -> B256 {
            let mut key = keccak256(self.next().to_be_bytes()).0;
            let shared = self.below(14) as usize;
            let base = keccak256([self.below(3) as u8]).0;
            for i in 0..shared {
                let nibble = if i % 2 == 0 { base[i / 2] >> 4 } else { base[i / 2] & 0xf };
                key[i / 2] = if i % 2 == 0 {
                    (key[i / 2] & 0x0f) | (nibble << 4)
                } else {
                    (key[i / 2] & 0xf0) | nibble
                };
            }
            B256::from(key)
        }

        fn value(&mut self) -> Vec<u8> {
            vec![1 + self.below(250) as u8; 1 + self.below(40) as usize]
        }
    }

    /// Applies row changes to stored rows.
    fn apply(rows: &mut BTreeMap<Nibbles, BranchNodeCompact>, updates: &NodeUpdates) {
        for (path, node) in updates {
            match node {
                Some(node) => rows.insert(*path, node.clone()),
                None => rows.remove(path),
            };
        }
    }

    /// Blocks of random changes on a trie kept across blocks (and sometimes dropped, to reveal
    /// from cold), checked against the specification: the root, and the stored rows after
    /// applying each block's row changes.
    #[test]
    fn blocks_of_changes_match_the_spec() {
        for seed in 0..60u64 {
            let mut rng = Rng(seed);
            let mut source = MemSource::default();
            let mut keys: Vec<B256> = Vec::new();
            let mut trie = LiveTrie::new(None);
            for block in 0..40 {
                if rng.below(5) == 0 {
                    trie = LiveTrie::new(None);
                }
                let mut next = source.leaves.clone();
                let mut updates = NodeUpdates::new();
                let ops = 1 + rng.below(if block % 9 == 8 { 300 } else { 30 });
                for _ in 0..ops {
                    let key = if !keys.is_empty() && rng.below(3) > 0 {
                        keys[rng.below(keys.len() as u64) as usize]
                    } else {
                        let key = rng.key();
                        keys.push(key);
                        key
                    };
                    let value = (rng.below(3) > 0).then(|| rng.value());
                    match &value {
                        Some(v) => next.insert(key, v.clone()),
                        None => next.remove(&key),
                    };
                    trie.update(&mut source, key, value, &mut updates).unwrap();
                }
                let root = trie.root(&mut source, &mut updates).unwrap();
                trie.check();
                let (expected_root, expected_rows) = Spec::trie(&next);
                assert_eq!(root, expected_root, "seed {seed} block {block}");
                apply(&mut source.rows, &updates);
                assert_eq!(source.rows, expected_rows, "seed {seed} block {block}: stored rows");
                source.leaves = next;
                // Reads through the trie agree with the leaves.
                for _ in 0..3 {
                    let key = keys[rng.below(keys.len() as u64) as usize];
                    assert_eq!(
                        trie.get(&mut source, key).unwrap(),
                        source.leaves.get(&key).cloned()
                    );
                }
            }
        }
    }

    type Rows = BTreeMap<Nibbles, BranchNodeCompact>;
    type Model = BTreeMap<B256, (Account, BTreeMap<B256, U256>)>;

    /// A stored state in memory: every trie's rows and leaves.
    #[derive(Debug, Default)]
    struct StateSource {
        accounts: MemSource,
        storages: BTreeMap<B256, MemSource>,
    }

    impl StateSource {
        fn trie(&mut self, owner: Option<B256>) -> &mut MemSource {
            match owner {
                None => &mut self.accounts,
                Some(owner) => self.storages.entry(owner).or_default(),
            }
        }
    }

    /// A [`StateSource`] shared by every thread.
    struct Shared<'a>(&'a StateSource);

    impl Shared<'_> {
        fn trie(&self, owner: Option<B256>) -> Option<&MemSource> {
            match owner {
                None => Some(&self.0.accounts),
                Some(owner) => self.0.storages.get(&owner),
            }
        }
    }

    impl LiveSource for Shared<'_> {
        fn branch(
            &mut self,
            owner: Option<B256>,
            path: &Nibbles,
        ) -> Result<Option<BranchNodeCompact>, TrieError> {
            Ok(self.trie(owner).and_then(|t| t.rows.get(path).cloned()))
        }

        fn first_branch_under(
            &mut self,
            owner: Option<B256>,
            prefix: &Nibbles,
        ) -> Result<Option<(Nibbles, BranchNodeCompact)>, TrieError> {
            Ok(self.trie(owner).and_then(|t| t.first_under(prefix)))
        }

        fn leaves(
            &mut self,
            owner: Option<B256>,
            prefix: &Nibbles,
            out: &mut Vec<(B256, Vec<u8>)>,
        ) -> Result<(), TrieError> {
            if let Some(trie) = self.trie(owner) {
                trie.leaves_under(prefix, out);
            }
            Ok(())
        }
    }

    /// The specification of a whole state: the root, the account rows and every storage trie's
    /// rows, with the leaves.
    fn spec_state(model: &Model) -> (B256, StateSource) {
        let mut out = StateSource::default();
        let mut account_leaves = BTreeMap::new();
        for (key, (account, storage)) in model {
            let leaves: BTreeMap<B256, Vec<u8>> =
                storage.iter().map(|(k, v)| (*k, storage_leaf(*v))).collect();
            let (root, rows) = Spec::trie(&leaves);
            if !rows.is_empty() || !leaves.is_empty() {
                out.storages.insert(*key, MemSource { leaves, rows });
            }
            account_leaves.insert(*key, account_leaf(account, root));
        }
        let (root, rows) = Spec::trie(&account_leaves);
        out.accounts = MemSource { leaves: account_leaves, rows };
        (root, out)
    }

    /// Blocks of account, storage, wipe and deletion changes applied to a state kept across
    /// blocks (sometimes dropped, to reveal from cold), checked against the specification: the
    /// state root, and every trie's stored rows after applying the block's trie updates.
    #[test]
    fn state_blocks_match_the_spec() {
        for seed in 0..40u64 {
            let mut rng = Rng(seed);
            let mut model = Model::new();
            let mut source = StateSource::default();
            let mut live = LiveState::default();
            let mut keys: Vec<B256> = Vec::new();
            for block in 0..30 {
                if rng.below(6) == 0 {
                    live = LiveState::default();
                }
                let mut state = HashedState::default();
                let mut next = model.clone();
                for _ in 0..1 + rng.below(if block % 7 == 6 { 120 } else { 25 }) {
                    let key = match next.keys().nth(rng.below(next.len().max(1) as u64) as usize) {
                        Some(existing) if rng.below(3) > 0 => *existing,
                        _ if !keys.is_empty() && rng.below(2) == 0 => {
                            keys[rng.below(keys.len() as u64) as usize]
                        }
                        _ => {
                            let key = rng.key();
                            keys.push(key);
                            key
                        }
                    };
                    match rng.below(10) {
                        0..=3 => {
                            let account = Account {
                                nonce: rng.below(3),
                                balance: U256::from(rng.below(1000) + 1),
                                bytecode_hash: (rng.below(3) == 0).then(|| B256::repeat_byte(7)),
                            };
                            state.set_account(key, account);
                            next.entry(key).or_insert((account, BTreeMap::new())).0 = account;
                        }
                        4..=7 if next.contains_key(&key) => {
                            let slot = match next[&key].1.keys().nth(rng.below(8) as usize) {
                                Some(existing) if rng.below(2) == 0 => *existing,
                                _ => rng.key(),
                            };
                            let value = if rng.below(4) == 0 {
                                U256::ZERO
                            } else {
                                U256::from(rng.below(5))
                            };
                            state.set_slot(key, slot, value);
                            let storage = &mut next.get_mut(&key).unwrap().1;
                            if value.is_zero() {
                                storage.remove(&slot);
                            } else {
                                storage.insert(slot, value);
                            }
                        }
                        8 if next.contains_key(&key) => {
                            state.wipe_storage(key);
                            next.get_mut(&key).unwrap().1.clear();
                        }
                        9 => {
                            state.delete_account(key);
                            next.remove(&key);
                        }
                        _ => {}
                    }
                }
                let threads = 1 + (seed % 4) as usize;
                let (root, updates) = {
                    let shared = &source;
                    let sources = move || -> Result<Box<dyn LiveSource + '_>, TrieError> {
                        Ok(Box::new(Shared(shared)))
                    };
                    live.apply(&mut Sources::Threads(&sources, threads), &state).unwrap()
                };
                let (expected_root, expected) = spec_state(&next);
                assert_eq!(root, expected_root, "seed {seed} block {block}");
                live.accounts.check();
                live.storages.values().for_each(LiveTrie::check);
                apply(&mut source.accounts.rows, &updates.account_nodes);
                for (owner, trie) in &updates.storage_tries {
                    let rows = &mut source.trie(Some(*owner)).rows;
                    if trie.wiped {
                        rows.clear();
                    }
                    apply(rows, &trie.nodes);
                }
                assert_eq!(
                    source.accounts.rows, expected.accounts.rows,
                    "seed {seed} block {block}: account rows"
                );
                let stored: BTreeMap<B256, Rows> = source
                    .storages
                    .iter()
                    .filter(|(_, s)| !s.rows.is_empty())
                    .map(|(k, s)| (*k, s.rows.clone()))
                    .collect();
                let wanted: BTreeMap<B256, Rows> = expected
                    .storages
                    .iter()
                    .filter(|(_, s)| !s.rows.is_empty())
                    .map(|(k, s)| (*k, s.rows.clone()))
                    .collect();
                assert_eq!(stored, wanted, "seed {seed} block {block}: storage rows");
                // The next block reads the new state.
                model = next;
                source.accounts.leaves = expected.accounts.leaves;
                for (owner, trie) in source.storages.iter_mut() {
                    trie.leaves =
                        expected.storages.get(owner).map(|s| s.leaves.clone()).unwrap_or_default();
                }
                for (owner, trie) in expected.storages {
                    source.storages.entry(owner).or_default().leaves = trie.leaves;
                }
            }
        }
    }
}
