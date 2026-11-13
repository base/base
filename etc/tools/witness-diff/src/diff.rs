use std::{
    collections::{HashMap, HashSet},
    fmt, fs,
    path::PathBuf,
};

use alloy_primitives::{Address, B256, Bytes, keccak256};
use alloy_provider::{Provider, RootProvider};
use alloy_rlp::Decodable;
use alloy_rpc_client::RpcClient;
use alloy_transport_http::{Client, Http};
use alloy_trie::{Nibbles, TrieAccount};
use base_common_network::Base;
use base_proof_mpt::TrieNode;
use eyre::Result;
use serde::{Deserialize, Deserializer};

/// Deserializes a vector while treating a JSON `null` as an empty vector.
pub fn deserialize_null_vec<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<Vec<T>>::deserialize(deserializer).map(Option::unwrap_or_default)
}

/// Execution witness format returned by reth/geth `debug_executionWitness`.
///
/// Both reth and geth return arrays (not maps) for `state`, `codes`, and `keys`.
/// Unknown fields (e.g. `headers`) are silently ignored by serde.
#[derive(Debug, Deserialize)]
pub struct Witness {
    /// State trie node preimages.
    #[serde(default, deserialize_with = "deserialize_null_vec")]
    pub state: Vec<Bytes>,
    /// Contract bytecodes.
    #[serde(default, deserialize_with = "deserialize_null_vec")]
    pub codes: Vec<Bytes>,
    /// MPT key preimages.
    #[serde(default, deserialize_with = "deserialize_null_vec")]
    pub keys: Vec<Bytes>,
}

/// Number of nibbles in a full trie key, which is always a keccak256 hash.
pub const FULL_PATH_NIBBLES: usize = 64;

/// Number of child slots in a branch node; the 17th list item is the unused branch value.
pub const BRANCH_CHILDREN: usize = 16;

/// Location of a leaf, reconstructed by walking the witness from a trie root.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum LeafPath {
    /// Account leaf at its full hashed-address path in the state trie.
    Account(Nibbles),
    /// Storage leaf at its full hashed-slot path in the storage trie of `account`.
    Storage {
        /// Full hashed-address path of the owning account.
        account: Nibbles,
        /// Full hashed-slot path within the account's storage trie.
        slot: Nibbles,
    },
    /// Leaf whose full path cannot be reconstructed because the witness lacks the nodes above it,
    /// or, for a storage leaf, a resolved owning account. Holds the path from the nearest root
    /// present in the witness.
    Unresolved(Nibbles),
}

impl LeafPath {
    /// Returns whether the full root-to-leaf path is known, so the leaf can be compared by path.
    pub const fn is_resolved(&self) -> bool {
        !matches!(self, Self::Unresolved(_))
    }

    /// Resolves the address of the account this leaf belongs to.
    ///
    /// Resolved paths use an exact hashed-address lookup; unresolved paths fall back to
    /// [`resolve_address`] suffix matching.
    pub fn address(&self, nibble_lookup: &HashMap<String, Address>) -> Option<Address> {
        match self {
            Self::Account(account) | Self::Storage { account, .. } => {
                nibble_lookup.get(&nibbles_hex(account)).copied()
            }
            Self::Unresolved(path) => resolve_address(nibble_lookup, &nibbles_hex(path)),
        }
    }
}

impl fmt::Display for LeafPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Account(account) => write!(f, "0x{}", nibbles_hex(account)),
            Self::Storage { account, slot } => {
                write!(f, "0x{}/0x{}", nibbles_hex(account), nibbles_hex(slot))
            }
            Self::Unresolved(path) => write!(f, "0x{} (partial)", nibbles_hex(path)),
        }
    }
}

/// A decoded trie leaf node with its reconstructed path.
#[derive(Debug)]
pub struct DecodedLeaf {
    /// Path of the leaf from its trie root.
    pub path: LeafPath,
    /// RLP-encoded value stored at this leaf.
    pub value: Bytes,
}

/// Which trie a walk is in, which determines how a leaf's path is classified.
#[derive(Debug, Clone, Copy)]
pub enum TrieKind {
    /// A node no other witness node references: the state root, or an orphaned subtrie.
    Unreferenced,
    /// A storage trie reached from an account leaf; `account` is `None` when that account's own
    /// path is unresolved.
    Storage {
        /// Full hashed-address path of the owning account, if resolved.
        account: Option<Nibbles>,
    },
}

/// Walks witness tries from their roots, collecting leaves with their full paths.
#[derive(Debug)]
pub struct LeafWalker<'a> {
    /// Decoded witness nodes keyed by the keccak256 hash of their preimage.
    pub nodes: &'a HashMap<B256, TrieNode>,
    /// Leaves collected so far.
    pub leaves: Vec<DecodedLeaf>,
}

impl LeafWalker<'_> {
    /// Visits `node`, located at `path` within a trie of the given `kind`.
    pub fn walk(&mut self, node: &TrieNode, path: Nibbles, kind: TrieKind) {
        match node {
            TrieNode::Empty => {}
            TrieNode::Blinded { commitment } => {
                if let Some(child) = self.nodes.get(commitment) {
                    self.walk(child, path, kind);
                }
            }
            TrieNode::Branch { stack } => {
                // Nibbles cannot exceed a full key; deeper paths only occur in malformed tries.
                if path.len() >= FULL_PATH_NIBBLES {
                    return;
                }
                for (nibble, child) in (0u8..).zip(stack.iter().take(BRANCH_CHILDREN)) {
                    let mut child_path = path;
                    child_path.push(nibble);
                    self.walk(child, child_path, kind);
                }
            }
            TrieNode::Extension { prefix, node } => {
                if path.len() + prefix.len() <= FULL_PATH_NIBBLES {
                    self.walk(node, path.join(prefix), kind);
                }
            }
            TrieNode::Leaf { prefix, value } => {
                if path.len() + prefix.len() <= FULL_PATH_NIBBLES {
                    self.visit_leaf(path.join(prefix), value, kind);
                }
            }
        }
    }

    /// Records a leaf at its full `path` and descends into the storage trie of account leaves.
    pub fn visit_leaf(&mut self, path: Nibbles, value: &Bytes, kind: TrieKind) {
        let is_full = path.len() == FULL_PATH_NIBBLES;
        let account = match kind {
            TrieKind::Unreferenced => try_decode_account(value),
            TrieKind::Storage { .. } => None,
        };
        let leaf_path = match kind {
            TrieKind::Unreferenced if is_full && account.is_some() => LeafPath::Account(path),
            TrieKind::Storage { account: Some(account) } if is_full => {
                LeafPath::Storage { account, slot: path }
            }
            _ => LeafPath::Unresolved(path),
        };
        self.leaves.push(DecodedLeaf { path: leaf_path, value: value.clone() });

        if let Some(storage_root) = account.and_then(|a| self.nodes.get(&a.storage_root)) {
            let owner = TrieKind::Storage { account: is_full.then_some(path) };
            self.walk(storage_root, Nibbles::default(), owner);
        }
    }
}

/// Index a slice of raw preimage bytes into a map keyed by their keccak256 hash.
pub fn index_by_hash(items: &[Bytes]) -> HashMap<B256, &Bytes> {
    items.iter().map(|b| (keccak256(b.as_ref()), b)).collect()
}

/// Build an address lookup map from key preimages.
///
/// Key preimages use RLP encoding: `0x94` prefix (`0x80 + 20`) indicates a
/// 20-byte address follows. The map is keyed by `keccak256(address)`.
pub fn build_address_lookup<'a>(keys: impl Iterator<Item = &'a Bytes>) -> HashMap<B256, Address> {
    let mut lookup = HashMap::new();
    for k in keys {
        if k.len() == 21 && k[0] == 0x94 {
            let addr = Address::from_slice(&k[1..]);
            lookup.insert(keccak256(addr), addr);
        }
    }
    lookup
}

/// Pre-encode the address lookup keys as nibble-hex strings for suffix matching.
///
/// Trie leaf paths are suffixes of the 64-nibble keccak256 hash of the key. We
/// store the full nibble-hex string so address resolution can call `ends_with`
/// without re-encoding the hash on every lookup.
pub fn build_nibble_address_lookup(
    address_lookup: &HashMap<B256, Address>,
) -> HashMap<String, Address> {
    address_lookup.iter().map(|(h, a)| (hex::encode(h), *a)).collect()
}

/// Look up an address by matching `prefix_hex` as a suffix of the hashed key.
///
/// Tries an O(1) direct lookup first (works when `prefix_hex` is the full
/// 64-nibble path). Falls back to an O(n) suffix scan for partial paths.
pub fn resolve_address(
    nibble_lookup: &HashMap<String, Address>,
    prefix_hex: &str,
) -> Option<Address> {
    if let Some(&addr) = nibble_lookup.get(prefix_hex) {
        return Some(addr);
    }
    nibble_lookup.iter().find(|(k, _)| k.ends_with(prefix_hex)).map(|(_, a)| *a)
}

/// Formats nibbles as a hex string with one character per nibble.
pub fn nibbles_hex(nibbles: &Nibbles) -> String {
    nibbles.iter().map(|n| format!("{n:x}")).collect()
}

/// Collects the hashes `node` references: hashed children and, for account leaves, the storage
/// root.
pub fn collect_references(node: &TrieNode, references: &mut HashSet<B256>) {
    match node {
        TrieNode::Empty => {}
        TrieNode::Blinded { commitment } => {
            references.insert(*commitment);
        }
        TrieNode::Branch { stack } => {
            stack.iter().for_each(|child| collect_references(child, references));
        }
        TrieNode::Extension { node, .. } => collect_references(node, references),
        TrieNode::Leaf { value, .. } => {
            if let Some(account) = try_decode_account(value) {
                references.insert(account.storage_root);
            }
        }
    }
}

/// Extract leaf nodes from a set of trie node preimages, with paths reconstructed from the roots.
///
/// A leaf node only stores the path remaining below its parent, so its full key is rebuilt by
/// walking from every node no other witness node references (normally just the pre-state root)
/// through branch and extension nodes, resolving hashed children against the witness. Storage
/// tries are walked from the storage roots of account leaves. Leaves below an orphaned subtrie, or
/// in a storage trie whose account path is unknown, are returned as [`LeafPath::Unresolved`].
pub fn extract_leaves(items: &[Bytes]) -> Vec<DecodedLeaf> {
    let decoded: Vec<(B256, TrieNode)> = items
        .iter()
        .filter_map(|data| {
            let node = TrieNode::decode(&mut data.as_ref()).ok()?;
            Some((keccak256(data), node))
        })
        .collect();

    let mut references = HashSet::new();
    decoded.iter().for_each(|(_, node)| collect_references(node, &mut references));

    let nodes: HashMap<B256, TrieNode> = decoded.iter().cloned().collect();
    let mut walker = LeafWalker { nodes: &nodes, leaves: Vec::new() };
    let mut walked_roots = HashSet::new();
    for (hash, node) in &decoded {
        if !references.contains(hash) && walked_roots.insert(*hash) {
            walker.walk(node, Nibbles::default(), TrieKind::Unreferenced);
        }
    }
    walker.leaves
}

/// Try to decode a leaf value as a `TrieAccount` (RLP-encoded account data).
pub fn try_decode_account(value: &Bytes) -> Option<TrieAccount> {
    TrieAccount::decode(&mut value.as_ref()).ok()
}

/// Find leaves that share the same full path but carry different values.
///
/// Leaves with an unresolved path are never paired.
pub fn find_differing_leaf_pairs(
    local_leaves: &[DecodedLeaf],
    rpc_leaves: &[DecodedLeaf],
) -> Vec<(LeafPath, Bytes, Bytes)> {
    let rpc_by_path: HashMap<&LeafPath, &Bytes> =
        rpc_leaves.iter().filter(|l| l.path.is_resolved()).map(|l| (&l.path, &l.value)).collect();

    let mut pairs = Vec::new();
    for local_leaf in local_leaves {
        if let Some(rpc_value) = rpc_by_path.get(&local_leaf.path)
            && local_leaf.value != **rpc_value
        {
            pairs.push((local_leaf.path.clone(), local_leaf.value.clone(), (*rpc_value).clone()));
        }
    }
    pairs
}

/// Returns the resolved leaves of `leaves` whose full path has no leaf in `other`.
pub fn leaves_missing_from<'a>(
    leaves: &'a [DecodedLeaf],
    other: &[DecodedLeaf],
) -> Vec<&'a DecodedLeaf> {
    let other_paths: HashSet<&LeafPath> = other.iter().map(|l| &l.path).collect();
    leaves.iter().filter(|l| l.path.is_resolved() && !other_paths.contains(&l.path)).collect()
}

/// Prints one leaf line, decoding the value as an account when possible.
pub fn print_leaf(label: &str, leaf: &DecodedLeaf, nibble_lookup: &HashMap<String, Address>) {
    if let Some(acct) = try_decode_account(&leaf.value) {
        let addr_str = leaf
            .path
            .address(nibble_lookup)
            .map(|a| a.to_string())
            .unwrap_or_else(|| String::from("(unknown)"));
        println!(
            "    {label} account {addr_str} path={}: nonce={}, balance={}, storage_root={}",
            leaf.path, acct.nonce, acct.balance, acct.storage_root
        );
    } else {
        println!("    {label} storage leaf path={}: 0x{}", leaf.path, hex::encode(&leaf.value));
    }
}

/// Compare two sets of bytes and print a compact diff summary.
///
/// Returns the total number of entries that differ (only-local + only-rpc).
pub fn compare_sets(label: &str, local_items: &[Bytes], rpc_items: &[Bytes]) -> usize {
    let local_map = index_by_hash(local_items);
    let rpc_map = index_by_hash(rpc_items);

    let only_local = local_map.keys().filter(|h| !rpc_map.contains_key(*h)).count();
    let only_rpc = rpc_map.keys().filter(|h| !local_map.contains_key(*h)).count();
    let common = local_map.keys().filter(|h| rpc_map.contains_key(*h)).count();

    println!("  {label}: {common} common, {only_local} only-local, {only_rpc} only-rpc");

    only_local + only_rpc
}

/// Run the witness diff tool.
///
/// Fetches the reference witness from the given RPC URL and compares it against
/// the local witness file, printing a human-readable diff of diverging trie nodes,
/// bytecodes, and account state.
pub async fn run(local: PathBuf, rpc_url: String, block: u64) -> Result<()> {
    // 1. Read local witness.
    println!("Reading local witness from: {}", local.display());
    let local_json = fs::read_to_string(&local)?;
    let local_witness: Witness = serde_json::from_str(&local_json)?;

    println!(
        "  Local: {} state nodes, {} codes, {} keys",
        local_witness.state.len(),
        local_witness.codes.len(),
        local_witness.keys.len()
    );

    // 2. Fetch RPC witness.
    println!("Fetching RPC witness for block #{block} from {rpc_url}");
    let url = rpc_url.parse()?;
    let http = Http::<Client>::new(url);
    let provider = RootProvider::<Base>::new(RpcClient::new(http, false));

    let block_hex = format!("0x{block:x}");
    let rpc_witness: Witness =
        provider.client().request("debug_executionWitness", &[&block_hex]).await?;

    println!(
        "  RPC:   {} state nodes, {} codes, {} keys",
        rpc_witness.state.len(),
        rpc_witness.codes.len(),
        rpc_witness.keys.len()
    );

    // 3. High-level comparison.
    println!("\n=== Overview ===");
    let state_diffs = compare_sets("state", &local_witness.state, &rpc_witness.state);
    let code_diffs = compare_sets("codes", &local_witness.codes, &rpc_witness.codes);
    let key_diffs = compare_sets("keys", &local_witness.keys, &rpc_witness.keys);

    // Merge both key sets for address lookups by chaining iterators — no clone needed.
    let address_lookup =
        build_address_lookup(local_witness.keys.iter().chain(rpc_witness.keys.iter()));
    let nibble_lookup = build_nibble_address_lookup(&address_lookup);

    println!("  address preimages available: {}", address_lookup.len());

    // 4. Extract leaves once — reused for both diff analysis and one-sided leaf counts.
    let local_leaves = extract_leaves(&local_witness.state);
    let rpc_leaves = extract_leaves(&rpc_witness.state);

    // 5. Find differing leaf nodes — this is the core analysis.
    println!("\n=== Differing Leaf Nodes ===");
    let leaf_pairs = find_differing_leaf_pairs(&local_leaves, &rpc_leaves);

    if leaf_pairs.is_empty() {
        println!("  No leaf nodes share the same path with different values.");
    } else {
        let mut account_diffs = Vec::new();
        let mut storage_diff_count = 0usize;

        for (path, local_value, rpc_value) in &leaf_pairs {
            let local_account = try_decode_account(local_value);
            let rpc_account = try_decode_account(rpc_value);

            if let (Some(local_acct), Some(rpc_acct)) = (local_account, rpc_account) {
                account_diffs.push((path, local_acct, rpc_acct));
            } else {
                storage_diff_count += 1;
            }
        }

        if account_diffs.is_empty() {
            println!("  No account leaves differ (all diffs are storage leaves).");
        } else {
            println!("  {} account(s) with different state:\n", account_diffs.len());
            for (path, local_acct, rpc_acct) in &account_diffs {
                println!("  ACCOUNT (path: {path})");
                if let Some(addr) = path.address(&nibble_lookup) {
                    println!("    Address:          {addr}");
                } else {
                    println!("    Address:          (unknown)");
                }
                println!(
                    "    Nonce:            {} (local) vs {} (rpc){}",
                    local_acct.nonce,
                    rpc_acct.nonce,
                    if local_acct.nonce == rpc_acct.nonce { " ✓" } else { " DIFFERS" }
                );
                println!(
                    "    Balance:          {} (local) vs {} (rpc){}",
                    local_acct.balance,
                    rpc_acct.balance,
                    if local_acct.balance == rpc_acct.balance { " ✓" } else { " DIFFERS" }
                );
                println!("    Code hash:        {} (local)", local_acct.code_hash);
                println!(
                    "                      {} (rpc){}",
                    rpc_acct.code_hash,
                    if local_acct.code_hash == rpc_acct.code_hash { " ✓" } else { " DIFFERS" }
                );
                println!("    Storage root:     {} (local)", local_acct.storage_root);
                println!(
                    "                      {} (rpc){}",
                    rpc_acct.storage_root,
                    if local_acct.storage_root == rpc_acct.storage_root {
                        " ✓"
                    } else {
                        " DIFFERS"
                    }
                );
                println!();
            }
        }

        println!(
            "  {storage_diff_count} storage leaf(s) also differ (values computed differently)."
        );
    }

    // 6. Count leaves only in one side (created/deleted accounts or storage slots).
    let leaves_only_local = leaves_missing_from(&local_leaves, &rpc_leaves);
    let leaves_only_rpc = leaves_missing_from(&rpc_leaves, &local_leaves);

    if !leaves_only_local.is_empty() || !leaves_only_rpc.is_empty() {
        println!("=== Leaves Only in One Witness ===");
        println!(
            "  {} leaves only in local, {} leaves only in RPC",
            leaves_only_local.len(),
            leaves_only_rpc.len()
        );
        for leaf in &leaves_only_local {
            print_leaf("LOCAL-ONLY", leaf, &nibble_lookup);
        }
        for leaf in &leaves_only_rpc {
            print_leaf("RPC-ONLY ", leaf, &nibble_lookup);
        }
        println!();
    }

    let unresolved_local: Vec<_> = local_leaves.iter().filter(|l| !l.path.is_resolved()).collect();
    let unresolved_rpc: Vec<_> = rpc_leaves.iter().filter(|l| !l.path.is_resolved()).collect();

    if !unresolved_local.is_empty() || !unresolved_rpc.is_empty() {
        println!("=== Leaves With Unresolved Paths ===");
        println!(
            "  {} unresolved in local, {} unresolved in RPC (not compared by path)",
            unresolved_local.len(),
            unresolved_rpc.len()
        );
        for leaf in &unresolved_local {
            print_leaf("LOCAL", leaf, &nibble_lookup);
        }
        for leaf in &unresolved_rpc {
            print_leaf("RPC  ", leaf, &nibble_lookup);
        }
        println!();
    }

    // 7. Summary.
    println!("=== SUMMARY ===");
    println!("State node diffs:     {state_diffs}");
    println!("Code diffs:           {code_diffs}");
    println!("Key diffs:            {key_diffs}");
    println!("Differing leaf pairs: {}", leaf_pairs.len());
    println!("Leaves only-local:    {}", leaves_only_local.len());
    println!("Leaves only-rpc:      {}", leaves_only_rpc.len());
    println!(
        "Unresolved leaves:    {} (local), {} (rpc)",
        unresolved_local.len(),
        unresolved_rpc.len()
    );

    let total_diffs = state_diffs
        + code_diffs
        + key_diffs
        + leaf_pairs.len()
        + leaves_only_local.len()
        + leaves_only_rpc.len();
    if total_diffs == 0 {
        println!("\nWitnesses are identical.");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use alloy_primitives::U256;
    use alloy_trie::EMPTY_ROOT_HASH;

    use super::*;

    const SHARED_SUFFIX: [u8; 63] = [0xa; 63];
    const SLOT_PATH: [u8; 64] = [0xb; 64];

    fn encode(node: &TrieNode) -> Bytes {
        alloy_rlp::encode(node).into()
    }

    fn account_value(nonce: u64, storage_root: B256) -> Bytes {
        let account =
            TrieAccount { nonce, balance: U256::ZERO, storage_root, code_hash: keccak256([]) };
        alloy_rlp::encode(account).into()
    }

    fn leaf(value: Bytes) -> TrieNode {
        TrieNode::Leaf { prefix: Nibbles::from_nibbles(SHARED_SUFFIX), value }
    }

    fn storage_leaf(value: u64) -> TrieNode {
        TrieNode::Leaf {
            prefix: Nibbles::from_nibbles(SLOT_PATH),
            value: alloy_rlp::encode(U256::from(value)).into(),
        }
    }

    /// Encodes a root branch with one hashed leaf child per entry, returning the witness preimages.
    fn witness_with_root_branch(children: &[(usize, TrieNode)]) -> Vec<Bytes> {
        let mut stack = vec![TrieNode::Empty; 17];
        let mut preimages = Vec::new();
        for (nibble, child) in children {
            stack[*nibble] = child.clone();
            preimages.push(encode(child));
        }
        preimages.insert(0, encode(&TrieNode::Branch { stack }));
        preimages
    }

    fn account_path(branch_nibble: u8) -> Nibbles {
        Nibbles::from_nibbles([branch_nibble].into_iter().chain(SHARED_SUFFIX).collect::<Vec<_>>())
    }

    #[test]
    fn leaves_with_equal_suffixes_under_different_branches_are_not_paired() {
        let local = witness_with_root_branch(&[(1, leaf(account_value(1, EMPTY_ROOT_HASH)))]);
        let rpc = witness_with_root_branch(&[(2, leaf(account_value(2, EMPTY_ROOT_HASH)))]);

        let pairs = find_differing_leaf_pairs(&extract_leaves(&local), &extract_leaves(&rpc));

        assert!(pairs.is_empty());
    }

    #[test]
    fn leaves_with_equal_suffixes_under_different_branches_are_each_only_in_one_witness() {
        let value = account_value(1, EMPTY_ROOT_HASH);
        let local = extract_leaves(&witness_with_root_branch(&[(1, leaf(value.clone()))]));
        let rpc = extract_leaves(&witness_with_root_branch(&[(2, leaf(value))]));

        let only_local: Vec<_> =
            leaves_missing_from(&local, &rpc).iter().map(|l| l.path.clone()).collect();
        let only_rpc: Vec<_> =
            leaves_missing_from(&rpc, &local).iter().map(|l| l.path.clone()).collect();

        assert_eq!(only_local, vec![LeafPath::Account(account_path(1))]);
        assert_eq!(only_rpc, vec![LeafPath::Account(account_path(2))]);
    }

    #[test]
    fn account_and_storage_leaves_at_the_same_full_path_are_paired() {
        let witness = |value| {
            let slot = storage_leaf(value);
            let account = leaf(account_value(1, keccak256(encode(&slot))));
            let mut preimages = witness_with_root_branch(&[(1, account)]);
            preimages.push(encode(&slot));
            preimages
        };

        let pairs =
            find_differing_leaf_pairs(&extract_leaves(&witness(1)), &extract_leaves(&witness(2)));

        let paths: Vec<_> = pairs.into_iter().map(|(path, ..)| path).collect();
        assert_eq!(
            paths,
            vec![
                LeafPath::Account(account_path(1)),
                LeafPath::Storage {
                    account: account_path(1),
                    slot: Nibbles::from_nibbles(SLOT_PATH)
                },
            ]
        );
    }

    #[test]
    fn leaves_without_their_parent_nodes_are_unresolved_and_not_compared() {
        let local = extract_leaves(&[encode(&leaf(account_value(1, EMPTY_ROOT_HASH)))]);
        let rpc = extract_leaves(&[encode(&leaf(account_value(2, EMPTY_ROOT_HASH)))]);

        assert!(local.iter().chain(&rpc).all(|l| !l.path.is_resolved()));
        assert!(find_differing_leaf_pairs(&local, &rpc).is_empty());
        assert!(leaves_missing_from(&local, &rpc).is_empty());
    }
}
