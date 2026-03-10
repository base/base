//! Integration test exercising the reth execution path (`OpEvmConfig` / `BasicBlockExecutor`)
//! for block #43127820 on Base Mainnet against **RPC-backed state**.
//!
//! Uses a provider-backed [`DatabaseRef`] that fetches account state, storage, and block
//! hashes on demand from the L2 archive RPC. After execution, computes the state root via
//! `TrieDB` and compares it against the on-chain and base-reth-node values.
//!
//! Run with:
//! ```sh
//! L2_RPC_URL=<base-archive-with-debug> cargo test --test rpc_backed_block_43127820 \
//!     -p base-execution-evm -- --nocapture
//! ```

use std::{collections::HashMap, fmt, fmt::Write, fs, sync::Arc};

use alloy_consensus::{Block, BlockBody, Header};
use alloy_eips::BlockId;
use alloy_primitives::{b256, keccak256, Address, Bytes, Sealable, B256, U256};
use alloy_provider::{network::primitives::BlockTransactions, Provider, RootProvider};
use alloy_rlp::Decodable;
use alloy_rpc_client::RpcClient;
use alloy_transport_http::{Client, Http};
use base_alloy_network::Base;
use base_execution_chainspec::OpChainSpecBuilder;
use base_execution_evm::{OpEvmConfig, OpRethReceiptBuilder};
use base_execution_primitives::OpTransactionSigned;
use base_proof_executor::{TrieDB, TrieDBProvider};
use base_proof_mpt::{NoopTrieHinter, TrieNode, TrieProvider};
use reth_evm::execute::{BasicBlockExecutor, Executor};
use reth_primitives_traits::RecoveredBlock;
use revm::context::DBErrorMarker;
use revm::database::CacheDB;
use revm::state::{AccountInfo, Bytecode};
use revm::{Database, DatabaseRef};
use serde::Deserialize;

/// Target block number (0x291f80c).
const TARGET_BLOCK: u64 = 43_127_820;

/// State root computed by `base-reth-node` in production (does NOT match on-chain).
const BASE_RETH_NODE_STATE_ROOT: B256 =
    b256!("be2b1f19054d1e7f8c0caf9532eb3d9b6ce0adb530058044ea64e6d2424edf21");

// ---------------------------------------------------------------------------
// RPC-backed database
// ---------------------------------------------------------------------------

/// Error type for [`RpcDb`] operations.
#[derive(Debug)]
struct RpcDbError(String);

impl fmt::Display for RpcDbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for RpcDbError {}
impl DBErrorMarker for RpcDbError {}

/// A [`DatabaseRef`] implementation backed by an L2 archive RPC.
///
/// Fetches account info (balance, nonce, code), storage slots, and block hashes
/// on demand at a fixed block height. Designed to be wrapped in [`CacheDB`] so
/// each unique query is only fetched once.
#[derive(Debug)]
struct RpcDb {
    provider: RootProvider<Base>,
    block_id: BlockId,
}

impl DatabaseRef for RpcDb {
    type Error = RpcDbError;

    fn basic_ref(&self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                let balance = self
                    .provider
                    .get_balance(address)
                    .block_id(self.block_id)
                    .await
                    .map_err(|e| RpcDbError(format!("get_balance({address}): {e}")))?;
                let nonce = self
                    .provider
                    .get_transaction_count(address)
                    .block_id(self.block_id)
                    .await
                    .map_err(|e| RpcDbError(format!("get_nonce({address}): {e}")))?;
                let code = self
                    .provider
                    .get_code_at(address)
                    .block_id(self.block_id)
                    .await
                    .map_err(|e| RpcDbError(format!("get_code({address}): {e}")))?;

                let code_hash = if code.is_empty() {
                    keccak256(b"")
                } else {
                    keccak256(&code)
                };

                Ok(Some(AccountInfo::new(
                    balance,
                    nonce,
                    code_hash,
                    Bytecode::new_raw(code),
                )))
            })
        })
    }

    fn storage_ref(&self, address: Address, index: U256) -> Result<U256, Self::Error> {
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                self.provider
                    .get_storage_at(address, index)
                    .block_id(self.block_id)
                    .await
                    .map_err(|e| {
                        RpcDbError(format!("get_storage({address}, {index}): {e}"))
                    })
            })
        })
    }

    fn block_hash_ref(&self, number: u64) -> Result<B256, Self::Error> {
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                let block = self
                    .provider
                    .get_block_by_number(number.into())
                    .await
                    .map_err(|e| {
                        RpcDbError(format!("get_block_hash({number}): {e}"))
                    })?;
                Ok(block.map(|b| b.header.hash).unwrap_or_default())
            })
        })
    }

    fn code_by_hash_ref(&self, _code_hash: B256) -> Result<Bytecode, Self::Error> {
        // Code is always provided inline via basic_ref's AccountInfo, so revm
        // should never call this. Return empty bytecode as a safe fallback.
        Ok(Bytecode::new())
    }
}

// ---------------------------------------------------------------------------
// Cached trie provider for state root computation
// ---------------------------------------------------------------------------

/// In-memory trie node provider pre-seeded by `eth_getProof` responses.
/// Falls back to `debug_dbGet` RPC for any nodes not in the cache.
struct CachedTrieProvider {
    nodes: std::sync::Mutex<HashMap<B256, Bytes>>,
    provider: RootProvider<Base>,
}

impl TrieProvider for CachedTrieProvider {
    type Error = RpcDbError;

    fn trie_node_by_hash(&self, key: B256) -> Result<TrieNode, Self::Error> {
        if let Some(data) = self.nodes.lock().unwrap().get(&key).cloned() {
            return TrieNode::decode(&mut data.as_ref())
                .map_err(|e| RpcDbError(format!("rlp decode: {e}")));
        }

        let data: Bytes = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                // Try 32-byte key first (geth hashdb), then 33-byte with 0x00
                // prefix (reth/pathdb scheme byte).
                let result = self
                    .provider
                    .client()
                    .request::<&[B256; 1], Bytes>("debug_dbGet", &[key])
                    .await;

                match result {
                    Ok(data) => Ok(data),
                    Err(_) => {
                        let prefixed: Bytes =
                            [&[0x00u8], key.as_slice()].concat().into();
                        self.provider
                            .client()
                            .request::<&[Bytes; 1], Bytes>(
                                "debug_dbGet",
                                &[prefixed],
                            )
                            .await
                            .map_err(|e| {
                                RpcDbError(format!("debug_dbGet({key}): {e}"))
                            })
                    }
                }
            })
        })?;

        self.nodes.lock().unwrap().insert(key, data.clone());
        TrieNode::decode(&mut data.as_ref())
            .map_err(|e| RpcDbError(format!("rlp decode: {e}")))
    }
}

impl TrieDBProvider for CachedTrieProvider {
    fn bytecode_by_hash(&self, code_hash: B256) -> Result<Bytes, Self::Error> {
        const CODE_PREFIX: u8 = b'c';

        if let Some(data) = self.nodes.lock().unwrap().get(&code_hash).cloned() {
            return Ok(data);
        }

        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                let code_key = [&[CODE_PREFIX], code_hash.as_slice()].concat();
                let result = self
                    .provider
                    .client()
                    .request::<&[Bytes; 1], Bytes>("debug_dbGet", &[code_key.into()])
                    .await;

                match result {
                    Ok(code) => Ok(code),
                    Err(_) => self
                        .provider
                        .client()
                        .request::<&[B256; 1], Bytes>("debug_dbGet", &[code_hash])
                        .await
                        .map_err(|e| {
                            RpcDbError(format!("bytecode({code_hash}): {e}"))
                        }),
                }
            })
        })
    }

    fn header_by_hash(&self, hash: B256) -> Result<Header, Self::Error> {
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                let data: Bytes = self
                    .provider
                    .client()
                    .request::<&[B256; 1], Bytes>("debug_getRawHeader", &[hash])
                    .await
                    .map_err(|e| RpcDbError(format!("header({hash}): {e}")))?;
                Header::decode(&mut data.as_ref())
                    .map_err(|e| RpcDbError(format!("header decode: {e}")))
            })
        })
    }
}

// ---------------------------------------------------------------------------
// Diff-mode trace types (for post-execution comparison)
// ---------------------------------------------------------------------------

/// Account state from `debug_traceBlockByNumber` with `prestateTracer`.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct PrestateAccount {
    #[serde(default)]
    balance: U256,
    #[serde(default)]
    nonce: u64,
    #[allow(dead_code)]
    #[serde(default)]
    code: Option<Bytes>,
    #[serde(default)]
    storage: Option<HashMap<B256, U256>>,
}

/// Single per-transaction diff trace result.
#[derive(Debug, Deserialize)]
struct DiffTraceResult {
    result: DiffResult,
}

/// Post state from a single transaction diff trace.
#[derive(Debug, Deserialize)]
struct DiffResult {
    post: HashMap<Address, PrestateAccount>,
}

// ---------------------------------------------------------------------------
// Test
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn block_43127820_reth_execution() {
    let rpc_url = match std::env::var("L2_RPC_URL") {
        Ok(url) => url,
        Err(_) => {
            eprintln!("L2_RPC_URL not set, skipping test");
            return;
        }
    };

    let url = rpc_url.parse().expect("Invalid RPC URL");
    let http = Http::<Client>::new(url);
    let provider = RootProvider::<Base>::new(RpcClient::new(http, false));

    // 1. Fetch target block and parent block.
    let target_block = provider
        .get_block_by_number(TARGET_BLOCK.into())
        .full()
        .await
        .expect("Failed to fetch target block")
        .expect("Target block not found");

    let parent_block = provider
        .get_block_by_number((TARGET_BLOCK - 1).into())
        .full()
        .await
        .expect("Failed to fetch parent block")
        .expect("Parent block not found");

    let expected_gas_used = target_block.header.inner.gas_used;
    let expected_state_root = target_block.header.inner.state_root;

    // 2. Create RPC-backed database at parent block.
    let parent_block_id = BlockId::from(TARGET_BLOCK - 1);
    let rpc_db = RpcDb {
        provider: provider.clone(),
        block_id: parent_block_id,
    };
    let cache_db = CacheDB::new(rpc_db);

    // 3. Extract transactions and signers from the target block.
    let full_transactions = match target_block.transactions {
        BlockTransactions::Full(txs) => txs,
        _ => panic!("Expected full transactions from .full() request"),
    };

    let tx_count = full_transactions.len();
    let mut transactions: Vec<OpTransactionSigned> = Vec::with_capacity(tx_count);
    let mut signers: Vec<Address> = Vec::with_capacity(tx_count);

    for tx in full_transactions {
        signers.push(tx.inner.inner.signer());
        transactions.push(tx.inner.inner.into_inner());
    }

    // 4. Build the block.
    let header: Header = target_block.header.inner.clone();
    let block = Block {
        header,
        body: BlockBody {
            transactions,
            ommers: vec![],
            withdrawals: Some(Default::default()),
        },
    };
    let recovered_block = RecoveredBlock::new_unhashed(block, signers);

    // 5. Execute via BasicBlockExecutor with RPC-backed state.
    let chain_spec = Arc::new(OpChainSpecBuilder::base_mainnet().build());
    let evm_config: OpEvmConfig =
        OpEvmConfig::new(chain_spec, OpRethReceiptBuilder::default());
    let executor = BasicBlockExecutor::new(evm_config, cache_db);

    eprintln!("Executing block #{TARGET_BLOCK} against RPC-backed state...");
    let output = executor
        .execute(&recovered_block)
        .expect("Block execution failed");

    assert_eq!(output.gas_used, expected_gas_used, "gas_used mismatch");
    assert_eq!(output.receipts.len(), tx_count, "receipt count mismatch");
    eprintln!(
        "Execution OK: gas_used={}, receipts={}",
        output.gas_used, tx_count
    );

    // 6. Compute state root from BundleState via TrieDB + eth_getProof.
    eprintln!(
        "Fetching eth_getProof for {} accounts...",
        output.state.state().len()
    );

    let mut trie_node_cache: HashMap<B256, Bytes> = HashMap::new();
    let mut proof_count = 0usize;

    for (addr, bundle_account) in output.state.state() {
        let slots: Vec<B256> = bundle_account
            .storage
            .keys()
            .map(|k| B256::from(k.to_be_bytes::<32>()))
            .collect();

        match provider
            .get_proof(*addr, slots)
            .block_id(parent_block_id)
            .await
        {
            Ok(proof) => {
                for node in &proof.account_proof {
                    trie_node_cache.insert(keccak256(node.as_ref()), node.clone());
                }
                for sp in &proof.storage_proof {
                    for node in &sp.proof {
                        trie_node_cache.insert(keccak256(node.as_ref()), node.clone());
                    }
                }
                proof_count += 1;
            }
            Err(e) => {
                eprintln!("  eth_getProof failed for {addr}: {e}");
            }
        }
    }
    eprintln!(
        "Pre-seeded {} trie nodes from {} account proofs",
        trie_node_cache.len(),
        proof_count,
    );

    let cached_provider = CachedTrieProvider {
        nodes: std::sync::Mutex::new(trie_node_cache),
        provider: provider.clone(),
    };
    let parent_header_sealed = parent_block.header.inner.clone().seal_slow();
    let mut trie_db = TrieDB::new(parent_header_sealed, cached_provider, NoopTrieHinter);

    // Pre-populate storage roots for all accounts in the BundleState.
    for (addr, account) in output.state.state() {
        if let Err(e) = trie_db.basic(*addr) {
            eprintln!(
                "  warn: basic({addr}) failed (status={:?}): {e}",
                account.status
            );
        }
    }

    // 7. Print state root comparison.
    eprintln!("\n=== State Root Comparison (block #{TARGET_BLOCK}) ===");
    eprintln!("On-chain (correct):    {expected_state_root}");
    eprintln!("base-reth-node (prod): {BASE_RETH_NODE_STATE_ROOT}");

    match trie_db.state_root(&output.state) {
        Ok(computed_state_root) => {
            eprintln!("This test (computed):  {computed_state_root}");
            eprintln!(
                "Matches on-chain:       {}",
                computed_state_root == expected_state_root
            );
            eprintln!(
                "Matches base-reth-node: {}",
                computed_state_root == BASE_RETH_NODE_STATE_ROOT
            );
        }
        Err(e) => {
            eprintln!("State root computation FAILED: {e}");
        }
    }

    // 8. Fetch on-chain state diff and write detailed comparison to file.
    let block_hex = format!("0x{:x}", TARGET_BLOCK);
    let diff_tracer_config = serde_json::json!({
        "tracer": "prestateTracer",
        "tracerConfig": { "diffMode": true }
    });

    let diff_traces: Vec<DiffTraceResult> = provider
        .client()
        .request(
            "debug_traceBlockByNumber",
            (&block_hex, &diff_tracer_config),
        )
        .await
        .expect("Failed to fetch diff traces");

    let mut block_post: HashMap<Address, PrestateAccount> = HashMap::new();
    for diff in diff_traces {
        for (addr, account) in diff.result.post {
            if let Some(existing) = block_post.get_mut(&addr) {
                if let Some(new_storage) = account.storage {
                    let slots = existing.storage.get_or_insert_with(HashMap::new);
                    for (key, value) in new_storage {
                        slots.insert(key, value);
                    }
                }
            } else {
                block_post.insert(addr, account);
            }
        }
    }

    // Build detailed report (written to file only, not stderr).
    let bundle_state = output.state.state();
    let mut mismatches = 0usize;
    let mut ok_count = 0usize;
    let mut report = String::new();

    writeln!(
        report,
        "=== BundleState vs On-Chain Diff (block #{TARGET_BLOCK}) ==="
    )
    .unwrap();
    writeln!(report, "Accounts in BundleState: {}", bundle_state.len()).unwrap();
    writeln!(
        report,
        "Accounts in on-chain diff: {}\n",
        block_post.len()
    )
    .unwrap();

    let mut bundle_addrs: Vec<Address> = bundle_state.keys().copied().collect();
    bundle_addrs.sort();

    for addr in &bundle_addrs {
        let bundle_account = &bundle_state[addr];
        let has_storage_changes = !bundle_account.storage.is_empty();
        let has_info_change = bundle_account.info != bundle_account.original_info;
        if !has_storage_changes && !has_info_change {
            continue;
        }

        if let Some(on_chain_post) = block_post.get(addr) {
            let mut issues = Vec::new();
            let on_chain_storage = on_chain_post.storage.as_ref();
            let mut storage_ok = 0usize;
            let mut storage_fail = 0usize;

            for (slot, storage_slot) in &bundle_account.storage {
                let slot_key = B256::from(slot.to_be_bytes::<32>());
                if let Some(oc_storage) = on_chain_storage {
                    if let Some(oc_value) = oc_storage.get(&slot_key) {
                        if storage_slot.present_value == *oc_value {
                            storage_ok += 1;
                        } else {
                            issues.push(format!(
                                "storage {slot_key}: reth={}, on-chain={}",
                                storage_slot.present_value, oc_value
                            ));
                            storage_fail += 1;
                        }
                    } else if storage_slot.present_value
                        != storage_slot.previous_or_original_value
                    {
                        issues.push(format!(
                            "storage {slot_key}: in BundleState but MISSING from on-chain diff",
                        ));
                        storage_fail += 1;
                    }
                } else if storage_slot.present_value
                    != storage_slot.previous_or_original_value
                {
                    issues.push(format!(
                        "storage {slot_key}: in BundleState but on-chain has NO storage changes",
                    ));
                    storage_fail += 1;
                }
            }

            if let Some(oc_storage) = on_chain_storage {
                for (slot_key, _) in oc_storage {
                    let slot_u256 = U256::from_be_bytes(slot_key.0);
                    if !bundle_account.storage.contains_key(&slot_u256) {
                        issues.push(format!(
                            "storage {slot_key}: MISSING from BundleState",
                        ));
                        storage_fail += 1;
                    }
                }
            }

            let total_storage = storage_ok + storage_fail;
            if issues.is_empty() {
                writeln!(report, "  OK   {addr}: {storage_ok}/{total_storage} slots match")
                    .unwrap();
                ok_count += 1;
            } else {
                writeln!(report, "  FAIL {addr}: status={:?}", bundle_account.status)
                    .unwrap();
                for issue in &issues {
                    writeln!(report, "       - {issue}").unwrap();
                }
                mismatches += issues.len();
            }
        } else {
            let storage_count = bundle_account.storage.len();
            writeln!(
                report,
                "  WARN {addr}: in BundleState ({storage_count} storage changes, status={:?}) \
                 but MISSING from on-chain diff",
                bundle_account.status,
            )
            .unwrap();
            mismatches += 1;
        }
    }

    let mut on_chain_only: Vec<Address> = block_post
        .keys()
        .filter(|addr| !bundle_state.contains_key(*addr))
        .copied()
        .collect();
    on_chain_only.sort();

    for addr in &on_chain_only {
        let on_chain_post = &block_post[addr];
        let storage_count = on_chain_post
            .storage
            .as_ref()
            .map(|s| s.len())
            .unwrap_or(0);
        writeln!(
            report,
            "  FAIL {addr}: MISSING from BundleState ({storage_count} storage changes)",
        )
        .unwrap();
        mismatches += 1;
    }

    writeln!(
        report,
        "\n=== RESULT: {ok_count} OK, {mismatches} mismatches found ==="
    )
    .unwrap();

    // Write report to file (not stderr — too verbose).
    let output_dir =
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/testdata");
    fs::create_dir_all(&output_dir).expect("Failed to create output directory");
    let output_path = output_dir.join("diff_report_block_43127820.txt");
    fs::write(&output_path, &report).expect("Failed to write diff report");
    eprintln!(
        "Diff report ({ok_count} OK, {mismatches} mismatches) written to: {}",
        output_path.display()
    );
}
