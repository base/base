use std::{collections::HashMap, fs, path::PathBuf};

use alloy_primitives::{Address, B256, Bytes, keccak256};
use alloy_provider::{Provider, RootProvider};
use alloy_rlp::Decodable;
use alloy_rpc_client::RpcClient;
use alloy_transport_http::{Client, Http};
use alloy_trie::TrieAccount;
use base_alloy_network::Base;
use base_proof_mpt::{Nibbles, TrieNode};
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

/// A decoded trie leaf node with its path.
#[derive(Debug)]
pub struct DecodedLeaf {
    /// Nibble-encoded path prefix of the leaf.
    pub prefix: Nibbles,
    /// RLP-encoded value stored at this leaf.
    pub value: Bytes,
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
pub fn resolve_address(
    nibble_lookup: &HashMap<String, Address>,
    prefix_hex: &str,
) -> Option<Address> {
    nibble_lookup.iter().find(|(nibbles, _)| nibbles.ends_with(prefix_hex)).map(|(_, a)| *a)
}

/// Extract leaf nodes from a set of trie node preimages.
pub fn extract_leaves(items: &[Bytes]) -> Vec<DecodedLeaf> {
    let mut leaves = Vec::new();
    for data in items {
        if let Ok(TrieNode::Leaf { prefix, value }) = TrieNode::decode(&mut data.as_ref()) {
            leaves.push(DecodedLeaf { prefix, value });
        }
    }
    leaves
}

/// Try to decode a leaf value as a `TrieAccount` (RLP-encoded account data).
pub fn try_decode_account(value: &Bytes) -> Option<TrieAccount> {
    TrieAccount::decode(&mut value.as_ref()).ok()
}

/// Find leaf nodes that share the same trie path but carry different values.
///
/// A well-formed trie has at most one leaf per path, so duplicate prefixes
/// within a single witness are not expected. If the input is malformed the
/// `HashMap::collect()` below will silently retain only the last entry for any
/// duplicate path; this is acceptable for a diagnostic tool where correctness
/// of the trie structure is a precondition.
pub fn find_differing_leaf_pairs(
    local_leaves: &[DecodedLeaf],
    rpc_leaves: &[DecodedLeaf],
) -> Vec<(Nibbles, Bytes, Bytes)> {
    let rpc_by_prefix: HashMap<&Nibbles, &Bytes> =
        rpc_leaves.iter().map(|l| (&l.prefix, &l.value)).collect();

    let mut pairs = Vec::new();
    for local_leaf in local_leaves {
        if let Some(rpc_value) = rpc_by_prefix.get(&local_leaf.prefix)
            && local_leaf.value != **rpc_value
        {
            pairs.push((local_leaf.prefix, local_leaf.value.clone(), (*rpc_value).clone()));
        }
    }
    pairs
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

        for (prefix, local_value, rpc_value) in &leaf_pairs {
            let local_account = try_decode_account(local_value);
            let rpc_account = try_decode_account(rpc_value);

            if let (Some(local_acct), Some(rpc_acct)) = (local_account, rpc_account) {
                account_diffs.push((*prefix, local_acct, rpc_acct));
            } else {
                storage_diff_count += 1;
            }
        }

        if account_diffs.is_empty() {
            println!("  No account leaves differ (all diffs are storage leaves).");
        } else {
            println!("  {} account(s) with different state:\n", account_diffs.len());
            for (prefix, local_acct, rpc_acct) in &account_diffs {
                let prefix_hex = prefix.iter().map(|n| format!("{n:x}")).collect::<String>();
                let found_address = resolve_address(&nibble_lookup, &prefix_hex);

                println!("  ACCOUNT (path suffix: 0x{prefix_hex})");
                if let Some(addr) = found_address {
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
    let rpc_prefixes: HashMap<&Nibbles, &Bytes> =
        rpc_leaves.iter().map(|l| (&l.prefix, &l.value)).collect();
    let local_prefixes: HashMap<&Nibbles, &Bytes> =
        local_leaves.iter().map(|l| (&l.prefix, &l.value)).collect();

    let leaves_only_local: Vec<_> =
        local_leaves.iter().filter(|l| !rpc_prefixes.contains_key(&l.prefix)).collect();
    let leaves_only_rpc: Vec<_> =
        rpc_leaves.iter().filter(|l| !local_prefixes.contains_key(&l.prefix)).collect();

    if !leaves_only_local.is_empty() || !leaves_only_rpc.is_empty() {
        println!("=== Leaves Only in One Witness ===");
        println!(
            "  {} leaves only in local, {} leaves only in RPC",
            leaves_only_local.len(),
            leaves_only_rpc.len()
        );

        for leaf in &leaves_only_local {
            let prefix_hex = leaf.prefix.iter().map(|n| format!("{n:x}")).collect::<String>();
            if let Some(acct) = try_decode_account(&leaf.value) {
                let addr_str = resolve_address(&nibble_lookup, &prefix_hex)
                    .map(|a| a.to_string())
                    .unwrap_or_else(|| String::from("(unknown)"));
                println!(
                    "    LOCAL-ONLY account {addr_str}: nonce={}, balance={}, storage_root={}",
                    acct.nonce, acct.balance, acct.storage_root
                );
            } else {
                println!(
                    "    LOCAL-ONLY storage leaf path=0x{prefix_hex}: 0x{}",
                    hex::encode(&leaf.value)
                );
            }
        }

        for leaf in &leaves_only_rpc {
            let prefix_hex = leaf.prefix.iter().map(|n| format!("{n:x}")).collect::<String>();
            if let Some(acct) = try_decode_account(&leaf.value) {
                let addr_str = resolve_address(&nibble_lookup, &prefix_hex)
                    .map(|a| a.to_string())
                    .unwrap_or_else(|| String::from("(unknown)"));
                println!(
                    "    RPC-ONLY  account {addr_str}: nonce={}, balance={}, storage_root={}",
                    acct.nonce, acct.balance, acct.storage_root
                );
            } else {
                println!(
                    "    RPC-ONLY  storage leaf path=0x{prefix_hex}: 0x{}",
                    hex::encode(&leaf.value)
                );
            }
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
