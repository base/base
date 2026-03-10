//! Integration test exercising the reth execution path (`OpEvmConfig` / `BasicBlockExecutor`)
//! for block #43127820 on Base Mainnet, including state root computation via `TrieDB`.
//!
//! This test fetches real on-chain data from an L2 archive RPC that supports
//! `debug_traceBlockByNumber`, populates a [`StateProviderTest`] with the pre-execution
//! state, runs block execution through [`BasicBlockExecutor`] -- the same path used
//! by `base-reth-node` -- and then computes the state root from the resulting `BundleState`
//! using the proof executor's `TrieDB` backed by `eth_getProof` data.
//!
//! Run with:
//! ```sh
//! L2_RPC_URL=<base-archive-with-debug> cargo test --test block_43127820_execution \
//!     -p base-execution-evm -- --nocapture
//! ```

use std::{collections::HashMap, fmt, fs, sync::Arc};

use alloy_consensus::{Block, BlockBody, Header};
use alloy_eips::{
    BlockId,
    eip2935::HISTORY_STORAGE_ADDRESS,
    eip4788::{BEACON_ROOTS_ADDRESS, SYSTEM_ADDRESS},
};
use alloy_primitives::{map::HashMap as PrimitivesMap, Address, Bytes, Sealable, B256, U256,
    keccak256,
};
use alloy_provider::{
    Provider, RootProvider,
    network::primitives::BlockTransactions,
};
use alloy_rlp::Decodable;
use alloy_rpc_client::RpcClient;
use alloy_transport_http::{Client, Http};
use base_alloy_network::Base;
use base_execution_chainspec::OpChainSpecBuilder;
use base_execution_evm::{OpEvmConfig, OpRethReceiptBuilder};
use base_execution_primitives::OpTransactionSigned;
use base_proof_executor::{TrieDB, TrieDBProvider};
use base_proof_mpt::{NoopTrieHinter, TrieNode, TrieProvider};
use base_revm::L1_BLOCK_CONTRACT;
use reth_evm::execute::{BasicBlockExecutor, Executor};
use reth_primitives_traits::{Account, RecoveredBlock};
use reth_revm::{database::StateProviderDatabase, test_utils::StateProviderTest};
use revm::Database;
use serde::Deserialize;

/// Target block number (0x291f80c).
const TARGET_BLOCK: u64 = 43_127_820;

// ---------------------------------------------------------------------------
// Prestate tracer types
// ---------------------------------------------------------------------------

/// Account state from `debug_traceBlockByNumber` with `prestateTracer`.
#[derive(Debug, Deserialize)]
struct PrestateAccount {
    #[serde(default)]
    balance: U256,
    #[serde(default)]
    nonce: u64,
    #[serde(default)]
    code: Option<Bytes>,
    #[serde(default)]
    storage: Option<HashMap<B256, U256>>,
}

/// Single per-transaction trace result.
#[derive(Debug, Deserialize)]
struct TraceResult {
    result: HashMap<Address, PrestateAccount>,
}

// ---------------------------------------------------------------------------
// Cached trie provider (pre-seeded by eth_getProof, fallback to debug_dbGet)
// ---------------------------------------------------------------------------

/// Error returned by [`CachedTrieProvider`].
#[derive(Debug)]
enum CachedTrieError {
    Rlp(alloy_rlp::Error),
    Rpc(String),
}

impl fmt::Display for CachedTrieError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rlp(e) => write!(f, "rlp decode error: {e}"),
            Self::Rpc(e) => write!(f, "rpc error: {e}"),
        }
    }
}

impl std::error::Error for CachedTrieError {}

/// In-memory trie node provider pre-seeded by `eth_getProof` responses.
/// Falls back to `debug_dbGet` RPC for any nodes not in the cache.
struct CachedTrieProvider {
    nodes: std::sync::Mutex<HashMap<B256, Bytes>>,
    provider: RootProvider<Base>,
}

impl TrieProvider for CachedTrieProvider {
    type Error = CachedTrieError;

    fn trie_node_by_hash(&self, key: B256) -> Result<TrieNode, Self::Error> {
        // Check the pre-seeded cache first.
        if let Some(data) = self.nodes.lock().unwrap().get(&key).cloned() {
            return TrieNode::decode(&mut data.as_ref()).map_err(CachedTrieError::Rlp);
        }

        // Fallback: fetch via debug_dbGet RPC.
        let data: Bytes = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                self.provider
                    .client()
                    .request::<&[B256; 1], Bytes>("debug_dbGet", &[key])
                    .await
                    .map_err(|e| {
                        CachedTrieError::Rpc(format!(
                            "debug_dbGet({key}): {e} -- \
                             the L2 RPC must support the debug namespace"
                        ))
                    })
            })
        })?;

        self.nodes.lock().unwrap().insert(key, data.clone());
        TrieNode::decode(&mut data.as_ref()).map_err(CachedTrieError::Rlp)
    }
}

impl TrieDBProvider for CachedTrieProvider {
    fn bytecode_by_hash(&self, code_hash: B256) -> Result<Bytes, Self::Error> {
        // geth hashdb scheme: code is stored with a 'c' prefix on the hash.
        const CODE_PREFIX: u8 = b'c';

        if let Some(data) = self.nodes.lock().unwrap().get(&code_hash).cloned() {
            return Ok(data);
        }

        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                // Try with geth code prefix first, then raw hash.
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
                        .map_err(|e| CachedTrieError::Rpc(format!("bytecode({code_hash}): {e}"))),
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
                    .map_err(|e| CachedTrieError::Rpc(format!("header({hash}): {e}")))?;
                Header::decode(&mut data.as_ref()).map_err(CachedTrieError::Rlp)
            })
        })
    }
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

    // 2. Fetch pre-execution state via debug_traceBlockByNumber with prestateTracer.
    let block_hex = format!("0x{:x}", TARGET_BLOCK);
    let tracer_config = serde_json::json!({
        "tracer": "prestateTracer",
        "tracerConfig": { "diffMode": false }
    });

    let traces: Vec<TraceResult> = provider
        .client()
        .request("debug_traceBlockByNumber", (&block_hex, &tracer_config))
        .await
        .expect("Failed to fetch prestate traces");

    // 3. Merge all per-tx prestates into a single map.
    //    For each address, the *first* occurrence represents the initial (pre-block) state.
    //    Later occurrences may reflect post-tx mutations, so we only fill in slots/code that
    //    we haven't seen yet.
    let mut prestate: HashMap<Address, PrestateAccount> = HashMap::new();
    for trace in traces {
        for (addr, account) in trace.result {
            if let Some(existing) = prestate.get_mut(&addr) {
                // Merge storage: only insert slots not yet seen.
                if let Some(new_storage) = account.storage {
                    let slots = existing.storage.get_or_insert_with(HashMap::new);
                    for (key, value) in new_storage {
                        slots.entry(key).or_insert(value);
                    }
                }
                // Take code if we don't have it yet.
                if existing.code.is_none() {
                    existing.code = account.code;
                }
            } else {
                prestate.insert(addr, account);
            }
        }
    }

    // 4. Pre-seed system addresses that are modified by apply_pre_execution_changes
    //    but are NOT captured by the prestate tracer (which only traces user transactions).
    //    Without these, EIP-4788/EIP-2935 system calls are no-ops and the BundleState diverges.
    let system_addrs = [
        SYSTEM_ADDRESS,
        BEACON_ROOTS_ADDRESS,
        HISTORY_STORAGE_ADDRESS,
        target_block.header.inner.beneficiary,
    ];
    let parent_block_id = BlockId::from(TARGET_BLOCK - 1);
    eprintln!("Pre-seeding {} system addresses...", system_addrs.len());
    for addr in &system_addrs {
        if prestate.contains_key(addr) {
            continue; // already in prestate from tracer
        }

        // Fetch account state at the parent block.
        let balance = provider
            .get_balance(*addr)
            .block_id(parent_block_id)
            .await
            .unwrap_or_default();
        let nonce = provider
            .get_transaction_count(*addr)
            .block_id(parent_block_id)
            .await
            .unwrap_or_default();
        let code = provider
            .get_code_at(*addr)
            .block_id(parent_block_id)
            .await
            .unwrap_or_default();

        let code_opt = if code.is_empty() { None } else { Some(code) };
        prestate.insert(*addr, PrestateAccount {
            balance,
            nonce,
            code: code_opt,
            storage: None,
        });
        eprintln!("  pre-seeded {addr} (nonce={nonce}, balance={balance}, has_code={})",
            prestate[addr].code.is_some());
    }

    // Also fetch storage for BEACON_ROOTS_ADDRESS at the slots the system call will read/write.
    // EIP-4788: writes at slots (timestamp % 8191) and (timestamp % 8191 + 8191).
    {
        let block_ts = target_block.header.inner.timestamp;
        const BEACON_ROOTS_HISTORY_BUFFER_LENGTH: u64 = 8191;
        let ts_idx = block_ts % BEACON_ROOTS_HISTORY_BUFFER_LENGTH;
        let root_idx = ts_idx + BEACON_ROOTS_HISTORY_BUFFER_LENGTH;
        let beacon_slots = vec![B256::from(U256::from(ts_idx)), B256::from(U256::from(root_idx))];

        let proof = provider
            .get_proof(BEACON_ROOTS_ADDRESS, beacon_slots)
            .block_id(parent_block_id)
            .await;
        if let Ok(proof) = proof {
            let storage: HashMap<B256, U256> = proof
                .storage_proof
                .iter()
                .map(|sp| (sp.key.as_b256(), sp.value))
                .collect();
            if let Some(acct) = prestate.get_mut(&BEACON_ROOTS_ADDRESS) {
                acct.storage = Some(storage);
            }
        }
    }

    // Fetch storage for HISTORY_STORAGE_ADDRESS at the slot the system call will write.
    // EIP-2935: writes parent hash at slot (parent_number % HISTORY_SERVE_WINDOW).
    {
        const HISTORY_SERVE_WINDOW: u64 = 8192;
        let slot = (TARGET_BLOCK - 1) % HISTORY_SERVE_WINDOW;
        let history_slots = vec![B256::from(U256::from(slot))];

        let proof = provider
            .get_proof(HISTORY_STORAGE_ADDRESS, history_slots)
            .block_id(parent_block_id)
            .await;
        if let Ok(proof) = proof {
            let storage: HashMap<B256, U256> = proof
                .storage_proof
                .iter()
                .map(|sp| (sp.key.as_b256(), sp.value))
                .collect();
            if let Some(acct) = prestate.get_mut(&HISTORY_STORAGE_ADDRESS) {
                acct.storage = Some(storage);
            }
        }
    }

    // 5. Populate StateProviderTest with the merged pre-block state.
    let mut db = StateProviderTest::default();
    for (addr, acct) in &prestate {
        let account = Account {
            nonce: acct.nonce,
            balance: acct.balance,
            bytecode_hash: None, // auto-computed by insert_account when code is Some
        };
        let bytecode = acct.code.clone();
        let storage: PrimitivesMap<B256, U256> =
            acct.storage.clone().unwrap_or_default().into_iter().collect();
        db.insert_account(*addr, account, bytecode, storage);
    }

    // Insert parent block hash for BLOCKHASH opcode lookups.
    db.insert_block_hash(TARGET_BLOCK - 1, parent_block.header.hash);

    // 5. Extract transactions and signers from the target block.
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

    // 6. Build the block.
    let header: Header = target_block.header.inner.clone();
    let block = Block {
        header,
        body: BlockBody {
            transactions,
            ommers: vec![],
            // Canyon+ requires withdrawals to be present (empty list).
            withdrawals: Some(Default::default()),
        },
    };
    let recovered_block = RecoveredBlock::new_unhashed(block, signers);

    // 7. Create executor using the same code path as base-reth-node.
    let chain_spec = Arc::new(OpChainSpecBuilder::base_mainnet().build());
    let evm_config: OpEvmConfig = OpEvmConfig::new(chain_spec, OpRethReceiptBuilder::default());
    let mut executor = BasicBlockExecutor::new(evm_config, StateProviderDatabase::new(&db));

    // Pre-load L1Block contract state into the cache.
    executor.with_state_mut(|state| {
        state.load_cache_account(L1_BLOCK_CONTRACT).unwrap();
    });

    // 8. Execute the block.
    let output = executor.execute(&recovered_block).expect("Block execution failed");

    // 9. Assertions on execution outputs.
    assert_eq!(output.gas_used, expected_gas_used, "gas_used mismatch");
    assert_eq!(output.receipts.len(), tx_count, "receipt count mismatch");
    eprintln!("Execution OK: gas_used={}, receipts={}", output.gas_used, tx_count);

    // 10. Compute state root from the BundleState using TrieDB + eth_getProof.
    //     Pre-seed trie node cache with proof nodes for every account/slot in the BundleState.
    eprintln!("Fetching eth_getProof for {} accounts...", output.state.state().len());

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
                if proof_count % 100 == 0 {
                    eprintln!("  fetched {proof_count}/{} proofs", output.state.state().len());
                }
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

    // Build TrieDB from the cached proof nodes and the parent header.
    let cached_provider = CachedTrieProvider {
        nodes: std::sync::Mutex::new(trie_node_cache),
        provider: provider.clone(),
    };
    let parent_header_sealed = parent_block.header.inner.clone().seal_slow();
    let mut trie_db = TrieDB::new(parent_header_sealed, cached_provider, NoopTrieHinter);

    // Pre-populate storage roots by reading each account from the trie.
    // TrieDB::state_root() defaults to EMPTY_ROOT_HASH for unknown accounts, which
    // causes KeyNotFound errors when deleting storage slots. Calling basic() for each
    // account in the BundleState forces TrieDB to open the account's trie path and
    // cache its actual storage root.
    eprintln!("Pre-populating storage roots for {} accounts...", output.state.state().len());
    for addr in output.state.state().keys() {
        if let Err(e) = trie_db.basic(*addr) {
            eprintln!("  warning: basic({addr}) failed: {e}");
        }
    }

    // Verify the trie root matches the parent state root BEFORE applying changes.
    // If this fails, the trie pre-population is wrong.
    let parent_state_root = parent_block.header.inner.state_root;
    let mut pre_root_check = trie_db.root().clone();
    let pre_root = pre_root_check.blind();
    eprintln!("Parent state root:    {parent_state_root}");
    eprintln!("Pre-update trie root: {pre_root}");
    if pre_root != parent_state_root {
        eprintln!("WARNING: trie root drifted during pre-population!");
    }

    // Compute the state root by applying the BundleState to the trie.
    let computed_state_root = trie_db
        .state_root(&output.state)
        .expect("Failed to compute state root from BundleState");

    eprintln!("Expected state root: {expected_state_root}");
    eprintln!("Computed state root: {computed_state_root}");

    // 11. Write execution results to a file for manual inspection.
    let output_dir =
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/testdata");
    fs::create_dir_all(&output_dir).expect("Failed to create output directory");

    let output_path = output_dir.join("execution_output_block_43127820.txt");
    let output_text = format!(
        "Block: {TARGET_BLOCK}\n\
         Gas Used: {}\n\
         Receipt Count: {}\n\
         Expected State Root: {expected_state_root}\n\
         Computed State Root: {computed_state_root}\n\
         State Root Match: {}\n\n\
         --- Receipts ---\n{:#?}\n\n\
         --- BundleState ---\n{:#?}",
        output.gas_used,
        output.receipts.len(),
        computed_state_root == expected_state_root,
        output.receipts,
        output.state,
    );
    fs::write(&output_path, &output_text).expect("Failed to write output");
    eprintln!("Output written to: {}", output_path.display());

    // 12. Assert state root matches.
    assert_eq!(computed_state_root, expected_state_root, "state root mismatch");
}
