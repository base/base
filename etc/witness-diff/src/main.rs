#![doc = include_str!("../README.md")]

use std::{collections::HashMap, fs, path::PathBuf};

use alloy_primitives::{keccak256, Address, Bytes, B256};
use alloy_provider::{Provider, RootProvider};
use alloy_rlp::Decodable;
use alloy_rpc_client::RpcClient;
use alloy_transport_http::{Client, Http};
use alloy_trie::TrieAccount;
use base_alloy_network::Base;
use base_proof_mpt::{Nibbles, TrieNode};
use clap::Parser;
use serde::Deserialize;

/// Compare two execution witnesses to find trie node differences.
///
/// Fetches one witness from an L2 RPC via `debug_executionWitness` and reads
/// the other from a local JSON file, then reports exactly which trie nodes,
/// codes, and keys differ — including a detailed breakdown of affected accounts.
#[derive(Parser, Debug)]
#[command(name = "witness-diff")]
struct Args {
    /// Path to the local witness JSON file (from re-execution).
    #[arg(long)]
    local: PathBuf,

    /// L2 RPC URL to fetch the reference witness from.
    #[arg(long)]
    rpc_url: String,

    /// Block number to fetch the witness for.
    #[arg(long)]
    block: u64,
}

/// Execution witness format (compatible with both geth and reth output).
#[derive(Debug, Deserialize)]
struct Witness {
    #[serde(default)]
    state: Option<Vec<Bytes>>,
    #[serde(default)]
    codes: Option<Vec<Bytes>>,
    #[serde(default)]
    keys: Option<Vec<Bytes>>,
    #[allow(dead_code)]
    #[serde(default)]
    headers: Option<Vec<serde_json::Value>>,
}

/// A decoded leaf node with its trie path.
struct DecodedLeaf {
    prefix: Nibbles,
    value: Bytes,
}

/// Index a list of raw preimage bytes into a map keyed by keccak256 hash.
fn index_by_hash(items: &[Bytes]) -> HashMap<B256, &Bytes> {
    items.iter().map(|b| (keccak256(b.as_ref()), b)).collect()
}

/// Build address lookup: keccak256(address) → address from key preimages.
/// Key preimages use RLP encoding: 0x94 prefix for 20-byte addresses.
fn build_address_lookup(keys: &[Bytes]) -> HashMap<B256, Address> {
    let mut lookup = HashMap::new();
    for k in keys {
        // RLP: 0x94 = 0x80 + 20, meaning a 20-byte string follows
        if k.len() == 21 && k[0] == 0x94 {
            let addr = Address::from_slice(&k[1..]);
            lookup.insert(keccak256(addr), addr);
        }
    }
    lookup
}

/// Extract leaf nodes from a set of trie node preimages.
fn extract_leaves(items: &[Bytes]) -> Vec<DecodedLeaf> {
    let mut leaves = Vec::new();
    for data in items {
        if let Ok(TrieNode::Leaf { prefix, value }) =
            TrieNode::decode(&mut data.as_ref())
        {
            leaves.push(DecodedLeaf { prefix, value });
        }
    }
    leaves
}

/// Try to decode a leaf value as a TrieAccount (RLP-encoded account data).
fn try_decode_account(value: &Bytes) -> Option<TrieAccount> {
    TrieAccount::decode(&mut value.as_ref()).ok()
}

/// Find leaf nodes that share the same trie path (prefix) but have different
/// values between local and RPC witnesses.
fn find_differing_leaf_pairs(
    local_items: &[Bytes],
    rpc_items: &[Bytes],
) -> Vec<(Nibbles, Bytes, Bytes)> {
    let local_leaves = extract_leaves(local_items);
    let rpc_leaves = extract_leaves(rpc_items);

    // Index RPC leaves by prefix for fast lookup.
    let rpc_by_prefix: HashMap<&Nibbles, &Bytes> = rpc_leaves
        .iter()
        .map(|l| (&l.prefix, &l.value))
        .collect();

    let mut pairs = Vec::new();
    for local_leaf in &local_leaves {
        if let Some(rpc_value) = rpc_by_prefix.get(&local_leaf.prefix) {
            if local_leaf.value != **rpc_value {
                pairs.push((
                    local_leaf.prefix.clone(),
                    local_leaf.value.clone(),
                    (*rpc_value).clone(),
                ));
            }
        }
    }
    pairs
}

/// Compare two sets of bytes and report diffs (compact summary).
fn compare_sets(
    label: &str,
    local_items: &[Bytes],
    rpc_items: &[Bytes],
) -> usize {
    let local_map = index_by_hash(local_items);
    let rpc_map = index_by_hash(rpc_items);

    let only_local = local_map
        .keys()
        .filter(|h| !rpc_map.contains_key(*h))
        .count();
    let only_rpc = rpc_map
        .keys()
        .filter(|h| !local_map.contains_key(*h))
        .count();
    let common = local_map
        .keys()
        .filter(|h| rpc_map.contains_key(*h))
        .count();

    println!(
        "  {label}: {common} common, {only_local} only-local, {only_rpc} only-rpc"
    );

    only_local + only_rpc
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    // 1. Read local witness.
    println!("Reading local witness from: {}", args.local.display());
    let local_json = fs::read_to_string(&args.local)?;
    let local: Witness = serde_json::from_str(&local_json)?;

    let local_state = local.state.unwrap_or_default();
    let local_codes = local.codes.unwrap_or_default();
    let local_keys = local.keys.unwrap_or_default();

    println!(
        "  Local: {} state nodes, {} codes, {} keys",
        local_state.len(),
        local_codes.len(),
        local_keys.len()
    );

    // 2. Fetch RPC witness.
    println!(
        "Fetching RPC witness for block #{} from {}",
        args.block, args.rpc_url
    );
    let url = args.rpc_url.parse()?;
    let http = Http::<Client>::new(url);
    let provider = RootProvider::<Base>::new(RpcClient::new(http, false));

    let block_hex = format!("0x{:x}", args.block);
    let rpc_witness: Witness = provider
        .client()
        .request("debug_executionWitness", &[&block_hex])
        .await?;

    let rpc_state = rpc_witness.state.unwrap_or_default();
    let rpc_codes = rpc_witness.codes.unwrap_or_default();
    let rpc_keys = rpc_witness.keys.unwrap_or_default();

    println!(
        "  RPC:   {} state nodes, {} codes, {} keys",
        rpc_state.len(),
        rpc_codes.len(),
        rpc_keys.len()
    );

    // 3. High-level comparison.
    println!("\n=== Overview ===");
    let state_diffs = compare_sets("state", &local_state, &rpc_state);
    let code_diffs = compare_sets("codes", &local_codes, &rpc_codes);

    // Merge both key sets for address lookups.
    let mut all_keys = local_keys.clone();
    all_keys.extend(rpc_keys);
    let address_lookup = build_address_lookup(&all_keys);

    println!(
        "  address preimages available: {}",
        address_lookup.len()
    );

    // 4. Find differing leaf nodes — this is the core analysis.
    println!("\n=== Differing Leaf Nodes ===");
    let leaf_pairs = find_differing_leaf_pairs(&local_state, &rpc_state);

    if leaf_pairs.is_empty() {
        println!("  No leaf nodes share the same path with different values.");
    } else {
        // Separate account leaves from storage leaves.
        let mut account_diffs = Vec::new();
        let mut storage_diff_count = 0usize;

        for (prefix, local_value, rpc_value) in &leaf_pairs {
            let local_account = try_decode_account(local_value);
            let rpc_account = try_decode_account(rpc_value);

            if let (Some(local_acct), Some(rpc_acct)) =
                (local_account, rpc_account)
            {
                account_diffs.push((prefix.clone(), local_acct, rpc_acct));
            } else {
                storage_diff_count += 1;
            }
        }

        // Print account diffs first — these are the most important.
        if account_diffs.is_empty() {
            println!("  No account leaves differ (all diffs are storage leaves).");
        } else {
            println!(
                "  {} account(s) with different state:\n",
                account_diffs.len()
            );
            for (prefix, local_acct, rpc_acct) in &account_diffs {
                let prefix_hex = prefix
                    .iter()
                    .map(|n| format!("{n:x}"))
                    .collect::<String>();

                // Find the address from key preimages.
                let mut found_address: Option<Address> = None;
                for (hashed, addr) in &address_lookup {
                    let hashed_nibbles = hex::encode(hashed);
                    if hashed_nibbles.ends_with(&prefix_hex) {
                        found_address = Some(*addr);
                        break;
                    }
                }

                println!("  ACCOUNT (path suffix: 0x{prefix_hex})");
                if let Some(addr) = found_address {
                    println!("    Address:          {addr}");
                } else {
                    println!("    Address:          (unknown)");
                }
                println!("    Nonce:            {} (local) vs {} (rpc){}",
                    local_acct.nonce, rpc_acct.nonce,
                    if local_acct.nonce == rpc_acct.nonce { " ✓" } else { " DIFFERS" });
                println!("    Balance:          {} (local) vs {} (rpc){}",
                    local_acct.balance, rpc_acct.balance,
                    if local_acct.balance == rpc_acct.balance { " ✓" } else { " DIFFERS" });
                println!("    Code hash:        {} (local)", local_acct.code_hash);
                println!("                      {} (rpc){}",
                    rpc_acct.code_hash,
                    if local_acct.code_hash == rpc_acct.code_hash { " ✓" } else { " DIFFERS" });
                println!("    Storage root:     {} (local)", local_acct.storage_root);
                println!("                      {} (rpc){}",
                    rpc_acct.storage_root,
                    if local_acct.storage_root == rpc_acct.storage_root { " ✓" } else { " DIFFERS" });
                println!();
            }
        }

        println!(
            "  {} storage leaf(s) also differ (values computed differently).",
            storage_diff_count
        );
    }

    // 5. Count leaves only in one side (created/deleted accounts or storage).
    let local_leaves = extract_leaves(&local_state);
    let rpc_leaves = extract_leaves(&rpc_state);
    let rpc_prefixes: HashMap<&Nibbles, &Bytes> = rpc_leaves
        .iter()
        .map(|l| (&l.prefix, &l.value))
        .collect();
    let local_prefixes: HashMap<&Nibbles, &Bytes> = local_leaves
        .iter()
        .map(|l| (&l.prefix, &l.value))
        .collect();

    let leaves_only_local: Vec<_> = local_leaves
        .iter()
        .filter(|l| !rpc_prefixes.contains_key(&l.prefix))
        .collect();
    let leaves_only_rpc: Vec<_> = rpc_leaves
        .iter()
        .filter(|l| !local_prefixes.contains_key(&l.prefix))
        .collect();

    if !leaves_only_local.is_empty() || !leaves_only_rpc.is_empty() {
        println!("=== Leaves Only in One Witness ===");
        println!(
            "  {} leaves only in local, {} leaves only in RPC",
            leaves_only_local.len(),
            leaves_only_rpc.len()
        );

        for leaf in &leaves_only_local {
            let prefix_hex = leaf
                .prefix
                .iter()
                .map(|n| format!("{n:x}"))
                .collect::<String>();
            if let Some(acct) = try_decode_account(&leaf.value) {
                let mut addr_str = String::from("(unknown)");
                for (hashed, addr) in &address_lookup {
                    if hex::encode(hashed).ends_with(&prefix_hex) {
                        addr_str = format!("{addr}");
                        break;
                    }
                }
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
            let prefix_hex = leaf
                .prefix
                .iter()
                .map(|n| format!("{n:x}"))
                .collect::<String>();
            if let Some(acct) = try_decode_account(&leaf.value) {
                let mut addr_str = String::from("(unknown)");
                for (hashed, addr) in &address_lookup {
                    if hex::encode(hashed).ends_with(&prefix_hex) {
                        addr_str = format!("{addr}");
                        break;
                    }
                }
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

    // 6. Summary.
    let total_diffs = state_diffs + code_diffs;
    println!("=== SUMMARY ===");
    println!("State node diffs:     {state_diffs}");
    println!("Code diffs:           {code_diffs}");
    println!("Differing leaf pairs: {}", leaf_pairs.len());
    println!("Leaves only-local:    {}", leaves_only_local.len());
    println!("Leaves only-rpc:      {}", leaves_only_rpc.len());

    if total_diffs == 0 {
        println!("\nWitnesses are identical.");
    }

    Ok(())
}
