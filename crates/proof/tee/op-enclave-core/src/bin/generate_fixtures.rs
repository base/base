//! Fixture generator for stateless execution integration tests.
//!
//! This binary generates test fixtures from Base Sepolia testnet by fetching
//! real block data, execution witnesses, and expected roots.
//!
//! # Usage
//!
//! ```bash
//! cargo run --bin generate_fixtures -- \
//!   --l2-rpc https://base-sepolia.cbhq.net \
//!   --l1-rpc https://sepolia.drpc.org \
//!   --block 37312300 \
//!   --output op-enclave-core/tests/fixtures/base_sepolia_block.json
//! ```
//!
//! # Requirements
//!
//! - Access to Base Sepolia L2 RPC endpoint
//! - Access to Sepolia L1 RPC endpoint
//! - The L2 node must support `debug_dbGet` RPC method

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use alloy_consensus::{Header, ReceiptEnvelope, Sealable};
use alloy_eips::eip2718::Decodable2718;
use alloy_primitives::{Address, B256, Bytes, keccak256};
use alloy_rlp::Decodable;
use clap::Parser;
use kona_executor::{StatelessL2Builder, TrieDBProvider};
use kona_mpt::{TrieNode, TrieProvider};
use kona_protocol::L1BlockInfoTx;
use op_alloy_consensus::OpTxEnvelope;
use serde::{Deserialize, Serialize};
use tokio::runtime::Handle;

use op_enclave_core::L1ChainConfig;
use op_enclave_core::executor::{
    EnclaveEvmFactory, EnclaveTrieHinter, ExecutionWitness, build_l1_block_info_from_deposit,
    extract_deposits_from_receipts, l2_block_to_block_info,
};
use op_enclave_core::providers::L2SystemConfigFetcher;
use op_enclave_core::types::account::AccountResult;

/// Command-line arguments for the fixture generator.
#[derive(Parser, Debug)]
#[command(name = "generate_fixtures")]
#[command(about = "Generate test fixtures from Base Sepolia testnet")]
struct Args {
    /// L2 RPC endpoint URL (e.g., https://sepolia.base.org)
    #[arg(long)]
    l2_rpc: String,

    /// L1 RPC endpoint URL (e.g., https://sepolia.drpc.org)
    #[arg(long)]
    l1_rpc: String,

    /// L2 block number to generate fixture for
    #[arg(long)]
    block: u64,

    /// Output file path (defaults to stdout if not specified)
    #[arg(long, short)]
    output: Option<PathBuf>,

    /// Pretty print JSON output
    #[arg(long, default_value = "true")]
    pretty: bool,
}

/// Complete test fixture for stateless execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StatelessTestFixture {
    /// The rollup configuration.
    pub rollup_config: kona_genesis::RollupConfig,

    /// The L1 chain configuration.
    pub l1_config: L1ChainConfig,

    /// The L1 origin block header.
    pub l1_origin: Header,

    /// The L1 origin block receipts.
    pub l1_receipts: Vec<ReceiptEnvelope>,

    /// Transactions from the previous L2 block (RLP-encoded).
    pub previous_block_txs: Vec<Bytes>,

    /// The L2 block header to validate.
    pub block_header: Header,

    /// Sequenced transactions for this block (RLP-encoded).
    pub sequenced_txs: Vec<Bytes>,

    /// Actual block transactions (RLP-encoded) from the real block.
    /// Used for bypass mode testing and deposit comparison.
    pub actual_block_txs: Vec<Bytes>,

    /// The execution witness.
    pub witness: ExecutionWitness,

    /// The L2ToL1MessagePasser account proof.
    pub message_account: AccountResult,

    /// Expected state root after execution.
    pub expected_state_root: B256,

    /// Expected receipts root after execution.
    pub expected_receipts_root: B256,
}

/// RPC response wrapper for JSON-RPC calls.
#[derive(Debug, Deserialize)]
struct RpcResponse<T> {
    #[allow(dead_code)]
    jsonrpc: String,
    #[allow(dead_code)]
    id: u64,
    result: Option<T>,
    error: Option<RpcError>,
}

/// RPC error response.
#[derive(Debug, Deserialize)]
struct RpcError {
    code: i64,
    message: String,
}

/// L2 block response from RPC (with tx hashes).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct L2BlockHashesResponse {
    hash: B256,
    parent_hash: B256,
    number: String,
    timestamp: String,
    state_root: B256,
    receipts_root: B256,
    transactions_root: B256,
    gas_limit: String,
    gas_used: String,
    #[serde(default)]
    base_fee_per_gas: Option<String>,
    transactions: Vec<B256>,
    miner: Address,
    logs_bloom: alloy_primitives::Bloom,
    extra_data: Bytes,
    mix_hash: B256,
    #[allow(dead_code)]
    nonce: String,
    #[serde(default)]
    parent_beacon_block_root: Option<B256>,
    #[serde(default)]
    withdrawals_root: Option<B256>,
}

/// L2 block with raw transaction bytes.
#[derive(Debug)]
struct L2BlockResponse {
    hash: B256,
    parent_hash: B256,
    number: String,
    timestamp: String,
    state_root: B256,
    receipts_root: B256,
    transactions_root: B256,
    gas_limit: u64,
    gas_used: u64,
    base_fee_per_gas: Option<u64>,
    transactions: Vec<Bytes>,
    beneficiary: Address,
    logs_bloom: alloy_primitives::Bloom,
    extra_data: Bytes,
    mix_hash: B256,
    parent_beacon_block_root: Option<B256>,
    withdrawals_root: Option<B256>,
}

/// Execution witness response from debug_executionWitness RPC.
#[derive(Debug, Deserialize)]
struct ExecutionWitnessResponse {
    headers: Vec<Header>,
    codes: Vec<String>,
    state: Vec<String>,
}

// =============================================================================
// CachingTrieProvider - Lazy-fetch trie nodes during execution
// =============================================================================

/// Error type for CachingTrieProvider.
#[derive(Debug, Clone)]
struct CachingTrieError(String);

impl std::fmt::Display for CachingTrieError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for CachingTrieError {}

/// Shared caches for witness data.
/// These can be cloned before creating a CachingTrieProvider and accessed after execution.
#[derive(Clone, Debug)]
struct SharedWitnessCaches {
    /// Cached state trie nodes: node_hash -> RLP-encoded node.
    state_cache: Arc<Mutex<HashMap<B256, Bytes>>>,
    /// Cached bytecode: code_hash -> bytecode.
    code_cache: Arc<Mutex<HashMap<B256, Bytes>>>,
}

impl SharedWitnessCaches {
    fn new() -> Self {
        Self {
            state_cache: Arc::new(Mutex::new(HashMap::new())),
            code_cache: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn prepopulate(&self, codes: HashMap<B256, Bytes>, state: HashMap<B256, Bytes>) {
        {
            let mut cache = self.state_cache.lock().unwrap();
            for (k, v) in state {
                cache.insert(k, v);
            }
        }
        {
            let mut cache = self.code_cache.lock().unwrap();
            for (k, v) in codes {
                cache.insert(k, v);
            }
        }
    }

    fn extract_state_cache(&self) -> HashMap<B256, Bytes> {
        self.state_cache.lock().unwrap().clone()
    }

    fn extract_code_cache(&self) -> HashMap<B256, Bytes> {
        self.code_cache.lock().unwrap().clone()
    }
}

/// A trie provider that fetches nodes on-demand via `debug_dbGet` RPC.
///
/// This implements the kona-mpt TrieProvider and kona-executor TrieDBProvider traits.
/// During block execution, it lazily fetches trie nodes and bytecode as they are accessed,
/// caching everything for later extraction as witness data.
#[derive(Debug)]
struct CachingTrieProvider {
    /// HTTP client for RPC calls.
    client: reqwest::Client,
    /// L2 RPC URL.
    rpc_url: String,
    /// Shared caches (reference-counted).
    caches: SharedWitnessCaches,
    /// The parent block header.
    parent_header: Header,
    /// Tokio runtime handle for blocking RPC calls.
    runtime_handle: Handle,
}

impl CachingTrieProvider {
    /// Creates a new CachingTrieProvider with the given shared caches.
    const fn new(
        client: reqwest::Client,
        rpc_url: String,
        parent_header: Header,
        runtime_handle: Handle,
        caches: SharedWitnessCaches,
    ) -> Self {
        Self {
            client,
            rpc_url,
            caches,
            parent_header,
            runtime_handle,
        }
    }

    /// Fetch a value from the node's database using debug_dbGet.
    fn fetch_db_value(&self, key: B256) -> Result<Option<Bytes>, CachingTrieError> {
        let client = self.client.clone();
        let rpc_url = self.rpc_url.clone();
        let handle = self.runtime_handle.clone();

        // Use block_in_place to allow blocking within an async context
        tokio::task::block_in_place(move || {
            handle.block_on(async {
                let response = client
                    .post(&rpc_url)
                    .json(&serde_json::json!({
                        "jsonrpc": "2.0",
                        "method": "debug_dbGet",
                        "params": [format!("{key:?}")],
                        "id": 1
                    }))
                    .send()
                    .await
                    .map_err(|e| CachingTrieError(format!("RPC request failed: {e}")))?;

                let rpc_response: RpcResponse<String> = response
                    .json()
                    .await
                    .map_err(|e| CachingTrieError(format!("Failed to parse RPC response: {e}")))?;

                if rpc_response.error.is_some() {
                    return Ok(None);
                }

                match rpc_response.result {
                    Some(hex_str) => {
                        let bytes = hex::decode(hex_str.trim_start_matches("0x"))
                            .map_err(|e| CachingTrieError(format!("Failed to decode hex: {e}")))?;
                        Ok(Some(Bytes::from(bytes)))
                    }
                    None => Ok(None),
                }
            })
        })
    }
}

impl TrieProvider for CachingTrieProvider {
    type Error = CachingTrieError;

    fn trie_node_by_hash(&self, key: B256) -> Result<TrieNode, Self::Error> {
        // Check cache first
        {
            let cache = self.caches.state_cache.lock().unwrap();
            if let Some(node_bytes) = cache.get(&key) {
                return TrieNode::decode(&mut node_bytes.as_ref())
                    .map_err(|e| CachingTrieError(format!("Failed to decode trie node: {e}")));
            }
        }

        // Fetch from RPC
        eprintln!("    [RPC] Fetching trie node: {key}");
        let node_bytes = self
            .fetch_db_value(key)?
            .ok_or_else(|| CachingTrieError(format!("trie node not found for hash: {key}")))?;

        // Verify hash matches
        let computed_hash = keccak256(&node_bytes);
        if computed_hash != key {
            return Err(CachingTrieError(format!(
                "hash mismatch: expected {key}, got {computed_hash}"
            )));
        }

        eprintln!(
            "    [RPC] Fetched {} bytes for node {key}",
            node_bytes.len()
        );

        // Cache it
        {
            let mut cache = self.caches.state_cache.lock().unwrap();
            cache.insert(key, node_bytes.clone());
        }

        // Decode and return
        TrieNode::decode(&mut node_bytes.as_ref())
            .map_err(|e| CachingTrieError(format!("Failed to decode trie node: {e}")))
    }
}

impl TrieDBProvider for CachingTrieProvider {
    fn bytecode_by_hash(&self, code_hash: B256) -> Result<Bytes, Self::Error> {
        // Check cache first
        {
            let cache = self.caches.code_cache.lock().unwrap();
            if let Some(bytecode) = cache.get(&code_hash) {
                return Ok(bytecode.clone());
            }
        }

        // For bytecode, geth stores it with a "c" prefix in the database
        // Try fetching with the code hash directly first
        if let Some(bytecode) = self.fetch_db_value(code_hash)? {
            // Verify hash
            let computed_hash = keccak256(&bytecode);
            if computed_hash == code_hash {
                let mut cache = self.caches.code_cache.lock().unwrap();
                cache.insert(code_hash, bytecode.clone());
                return Ok(bytecode);
            }
        }

        Err(CachingTrieError(format!(
            "bytecode not found for hash: {code_hash}"
        )))
    }

    fn header_by_hash(&self, hash: B256) -> Result<Header, Self::Error> {
        let parent_hash = self.parent_header.hash_slow();
        if hash == parent_hash {
            Ok(self.parent_header.clone())
        } else {
            Err(CachingTrieError(format!(
                "header not found for hash: {hash}"
            )))
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    let block = args.block;
    eprintln!("Generating fixture for block {block} ...");
    eprintln!("L2 RPC: {}", args.l2_rpc);
    eprintln!("L1 RPC: {}", args.l1_rpc);

    // Create HTTP client
    let client = reqwest::Client::new();

    // Get tokio runtime handle for blocking RPC calls
    let runtime_handle = Handle::current();

    // 1. Fetch L2 block header
    eprintln!("Fetching L2 block {block}...");
    let block_response = fetch_l2_block(&client, &args.l2_rpc, block).await?;
    let block_hash = block_response.hash;
    eprintln!("  Block hash: {block_hash:?}");

    // 2. Fetch previous L2 block
    let prev_block = block - 1;
    eprintln!("Fetching previous L2 block {prev_block}...");
    let prev_block_response = fetch_l2_block(&client, &args.l2_rpc, prev_block).await?;
    let prev_hash = prev_block_response.hash;
    eprintln!("  Previous block hash: {prev_hash:?}");

    // 3. Fetch execution witness using debug_executionWitness (for initial codes/state)
    eprintln!("Fetching initial execution witness...");
    let witness_response = fetch_execution_witness(&client, &args.l2_rpc, block).await?;
    let initial_codes_len = witness_response.codes.len();
    let initial_state_len = witness_response.state.len();
    eprintln!("  Initial codes: {initial_codes_len}, state nodes: {initial_state_len}");

    // 4. Get L1 origin from L1 info deposit tx in previous block
    eprintln!("Extracting L1 origin from previous block...");
    let l1_origin_hash = extract_l1_origin_hash(&prev_block_response)?;
    eprintln!("  L1 origin hash: {l1_origin_hash:?}");

    // 5. Fetch L1 origin block and receipts
    eprintln!("Fetching L1 origin block...");
    let l1_origin = fetch_l1_block(&client, &args.l1_rpc, l1_origin_hash).await?;
    let l1_number = l1_origin.number;
    eprintln!("  L1 block number: {l1_number}");

    eprintln!("Fetching L1 receipts...");
    let l1_receipts = fetch_l1_receipts(&client, &args.l1_rpc, l1_origin_hash).await?;
    let receipts_len = l1_receipts.len();
    eprintln!("  L1 receipts: {receipts_len}");

    // 6. Fetch message account proof
    eprintln!("Fetching message account proof...");
    let message_passer_address = "0x4200000000000000000000000000000000000016".parse::<Address>()?;
    let message_account =
        fetch_account_proof(&client, &args.l2_rpc, message_passer_address, block).await?;

    // 7. Convert initial witness to HashMap<B256, Bytes> format
    let initial_codes: HashMap<B256, Bytes> = witness_response
        .codes
        .iter()
        .filter_map(|code_hex| {
            let code_bytes = hex::decode(code_hex.trim_start_matches("0x")).ok()?;
            let code_hash = keccak256(&code_bytes);
            Some((code_hash, Bytes::from(code_bytes)))
        })
        .collect();

    let initial_state: HashMap<B256, Bytes> = witness_response
        .state
        .iter()
        .filter_map(|node_hex| {
            let node_bytes = hex::decode(node_hex.trim_start_matches("0x")).ok()?;
            let node_hash = keccak256(&node_bytes);
            Some((node_hash, Bytes::from(node_bytes)))
        })
        .collect();

    // 8. Build the block header
    let block_header = block_response_to_header(&block_response)?;
    let previous_header = block_response_to_header(&prev_block_response)?;

    // 9. Create shared caches and prepopulate with initial witness
    eprintln!("Creating shared witness caches...");
    let shared_caches = SharedWitnessCaches::new();
    shared_caches.prepopulate(initial_codes, initial_state);

    // Create CachingTrieProvider with shared caches
    let caching_provider = CachingTrieProvider::new(
        client.clone(),
        args.l2_rpc.clone(),
        previous_header.clone(),
        runtime_handle,
        shared_caches.clone(), // Clone the Arc-backed caches
    );

    // 10. Build payload attributes for execution
    let rollup_config = get_base_sepolia_rollup_config();
    let l1_config = get_sepolia_l1_config();

    // Get L2 parent info and system config
    let first_prev_tx_bytes = prev_block_response
        .transactions
        .first()
        .ok_or("Previous block has no transactions")?;
    let first_prev_tx = OpTxEnvelope::decode_2718(&mut first_prev_tx_bytes.as_ref())
        .map_err(|e| format!("Failed to decode previous block deposit tx: {e}"))?;
    let first_prev_tx_calldata = match &first_prev_tx {
        OpTxEnvelope::Deposit(d) => Some(d.input.clone()),
        _ => return Err("First previous tx is not a deposit".into()),
    };

    let l2_fetcher = L2SystemConfigFetcher::new(
        rollup_config.clone(),
        prev_hash,
        previous_header.clone(),
        first_prev_tx_calldata.clone(),
    );
    let system_config = l2_fetcher
        .system_config_by_l2_hash(prev_hash)
        .map_err(|e| format!("Failed to get system config: {e}"))?;

    let l2_parent = l2_block_to_block_info(
        &rollup_config,
        &previous_header,
        prev_hash,
        first_prev_tx_bytes,
    )
    .map_err(|e| format!("Failed to get L2 parent info: {e}"))?;

    // Determine if we need deposits from this L1 block
    let include_deposits = l2_parent.l1_origin.hash != l1_origin_hash;
    let sequence_number = if include_deposits {
        0
    } else {
        l2_parent.seq_num + 1
    };

    eprintln!("  Include deposits: {include_deposits}, sequence: {sequence_number}");

    // For the fixture, we need sequenced txs (excluding deposits)
    let sequenced_txs = extract_sequenced_txs(&block_response);

    // Regenerate deposits using the same logic as execute_stateless
    // This ensures the fixture captures the exact trie nodes needed for test execution
    let receipts_for_deposits: &[ReceiptEnvelope] =
        if include_deposits { &l1_receipts } else { &[] };
    let generated_deposits = extract_deposits_from_receipts(
        &rollup_config,
        &l1_config,
        &system_config,
        &l1_origin,
        l1_origin_hash,
        receipts_for_deposits,
        block_header.number,
        block_header.timestamp,
        sequence_number,
    )
    .map_err(|e| format!("Failed to extract deposits: {e}"))?;

    // Combine deposits + sequenced transactions (same as test does)
    let all_txs: Vec<Bytes> = [generated_deposits.clone(), sequenced_txs.clone()].concat();

    eprintln!(
        "  Generated deposits: {}, sequenced txs: {}",
        generated_deposits.len(),
        sequenced_txs.len()
    );

    // Build L1BlockInfo from the previous block's deposit (same as test does)
    let spec_id = rollup_config.spec_id(block_header.timestamp);
    let l1_block_info = build_l1_block_info_from_deposit(
        first_prev_tx_calldata
            .as_ref()
            .ok_or("No deposit calldata")?,
        spec_id,
    )
    .map_err(|e| format!("Failed to build L1BlockInfo: {e}"))?;

    // 11. Execute block with CachingTrieProvider to capture all accessed nodes
    eprintln!("Executing block to capture witness...");
    use alloy_primitives::B64;
    use alloy_rpc_types_engine::PayloadAttributes;
    use op_alloy_rpc_types_engine::OpPayloadAttributes;

    let parent_sealed = previous_header.seal_slow();

    // Extract EIP-1559 params from extra_data for Holocene+
    let eip_1559_params = if rollup_config.is_holocene_active(block_header.timestamp) {
        block_header
            .extra_data
            .get(1..9)
            .and_then(|s| <[u8; 8]>::try_from(s).ok())
            .map(B64::from)
    } else {
        None
    };

    // Extract min_base_fee from extra_data for Jovian+
    let min_base_fee = if rollup_config.is_jovian_active(block_header.timestamp) {
        block_header
            .extra_data
            .get(9..17)
            .and_then(|s| <[u8; 8]>::try_from(s).ok())
            .map(u64::from_be_bytes)
    } else {
        None
    };

    let attrs = OpPayloadAttributes {
        payload_attributes: PayloadAttributes {
            timestamp: block_header.timestamp,
            prev_randao: block_header.mix_hash,
            suggested_fee_recipient: block_header.beneficiary,
            withdrawals: None,
            parent_beacon_block_root: block_header.parent_beacon_block_root,
        },
        transactions: Some(all_txs),
        no_tx_pool: Some(true),
        gas_limit: Some(block_header.gas_limit),
        eip_1559_params,
        min_base_fee,
    };

    let evm_factory = EnclaveEvmFactory::new(l1_block_info);
    let mut builder = StatelessL2Builder::new(
        &rollup_config,
        evm_factory,
        caching_provider,
        EnclaveTrieHinter,
        parent_sealed,
    );

    let outcome = builder
        .build_block(attrs)
        .map_err(|e| format!("Block execution failed: {e}"))?;

    eprintln!("  Computed state root: {:?}", outcome.header.state_root);
    eprintln!("  Expected state root: {:?}", block_response.state_root);
    eprintln!(
        "  Computed receipts root: {:?}",
        outcome.header.receipts_root
    );
    eprintln!(
        "  Expected receipts root: {:?}",
        block_response.receipts_root
    );

    // Verify execution results
    let mut validation_errors = Vec::new();
    if outcome.header.state_root != block_response.state_root {
        eprintln!(
            "  WARNING: State root mismatch: expected {:?}, got {:?}",
            block_response.state_root, outcome.header.state_root
        );
        validation_errors.push("state_root");
    }
    if outcome.header.receipts_root != block_response.receipts_root {
        eprintln!(
            "  WARNING: Receipts root mismatch: expected {:?}, got {:?}",
            block_response.receipts_root, outcome.header.receipts_root
        );
        validation_errors.push("receipts_root");
    }

    if validation_errors.is_empty() {
        eprintln!("  Execution successful - all roots match!");
    } else {
        eprintln!(
            "  Execution completed with {} validation warning(s)",
            validation_errors.len()
        );
        eprintln!("  Continuing to generate fixture with captured witness data...");
    }

    // 12. Extract cached witness data from the shared caches
    // The shared_caches still holds all nodes accessed during execution (including any fetched via debug_dbGet)
    eprintln!("Extracting witness from execution caches...");

    let final_state = shared_caches.extract_state_cache();
    let final_codes = shared_caches.extract_code_cache();

    eprintln!(
        "  Final codes: {}, state nodes: {}",
        final_codes.len(),
        final_state.len()
    );

    // Convert to HashMap<String, String> format for serialization
    let codes_map: HashMap<String, String> = final_codes
        .iter()
        .map(|(hash, bytes)| (format!("{hash:?}"), format!("0x{}", hex::encode(bytes))))
        .collect();

    let state_map: HashMap<String, String> = final_state
        .iter()
        .map(|(hash, bytes)| (format!("{hash:?}"), format!("0x{}", hex::encode(bytes))))
        .collect();

    // 13. Build the fixture
    // Store actual block transactions for comparison/bypass testing
    let actual_block_txs = block_response.transactions.clone();

    let fixture = StatelessTestFixture {
        rollup_config,
        l1_config,
        l1_origin,
        l1_receipts,
        previous_block_txs: prev_block_response.transactions,
        block_header,
        sequenced_txs,
        actual_block_txs,
        witness: ExecutionWitness {
            headers: witness_response.headers,
            codes: codes_map,
            state: state_map,
        },
        message_account,
        expected_state_root: block_response.state_root,
        expected_receipts_root: block_response.receipts_root,
    };

    // 14. Output the fixture
    let json = if args.pretty {
        serde_json::to_string_pretty(&fixture)?
    } else {
        serde_json::to_string(&fixture)?
    };

    if let Some(output_path) = args.output {
        fs::write(&output_path, &json)?;
        eprintln!("Fixture written to: {output_path:?}");
    } else {
        println!("{json}");
    }

    eprintln!("Done!");
    Ok(())
}

/// Fetch an L2 block by number with raw transaction bytes.
async fn fetch_l2_block(
    client: &reqwest::Client,
    rpc_url: &str,
    block_number: u64,
) -> Result<L2BlockResponse, Box<dyn std::error::Error>> {
    // First fetch block with transaction hashes
    let response = client
        .post(rpc_url)
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "method": "eth_getBlockByNumber",
            "params": [format!("0x{block_number:x}"), false],
            "id": 1
        }))
        .send()
        .await?
        .json::<RpcResponse<L2BlockHashesResponse>>()
        .await?;

    if let Some(error) = response.error {
        return Err(format!("RPC error {}: {}", error.code, error.message).into());
    }

    let block = response.result.ok_or("Block not found")?;

    // Fetch raw transactions
    let mut raw_txs = Vec::with_capacity(block.transactions.len());
    for tx_hash in &block.transactions {
        let raw_tx = fetch_raw_transaction(client, rpc_url, *tx_hash).await?;
        raw_txs.push(raw_tx);
    }

    // Parse hex values
    let gas_limit = u64::from_str_radix(block.gas_limit.trim_start_matches("0x"), 16)?;
    let gas_used = u64::from_str_radix(block.gas_used.trim_start_matches("0x"), 16)?;
    let base_fee = block
        .base_fee_per_gas
        .as_ref()
        .map(|s| u64::from_str_radix(s.trim_start_matches("0x"), 16))
        .transpose()?;

    Ok(L2BlockResponse {
        hash: block.hash,
        parent_hash: block.parent_hash,
        number: block.number,
        timestamp: block.timestamp,
        state_root: block.state_root,
        receipts_root: block.receipts_root,
        transactions_root: block.transactions_root,
        gas_limit,
        gas_used,
        base_fee_per_gas: base_fee,
        transactions: raw_txs,
        beneficiary: block.miner,
        logs_bloom: block.logs_bloom,
        extra_data: block.extra_data,
        mix_hash: block.mix_hash,
        parent_beacon_block_root: block.parent_beacon_block_root,
        withdrawals_root: block.withdrawals_root,
    })
}

/// Fetch raw transaction by hash.
async fn fetch_raw_transaction(
    client: &reqwest::Client,
    rpc_url: &str,
    tx_hash: B256,
) -> Result<Bytes, Box<dyn std::error::Error>> {
    let response = client
        .post(rpc_url)
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "method": "eth_getRawTransactionByHash",
            "params": [format!("{tx_hash:?}")],
            "id": 1
        }))
        .send()
        .await?
        .json::<RpcResponse<Bytes>>()
        .await?;

    if let Some(error) = response.error {
        return Err(format!("RPC error {}: {}", error.code, error.message).into());
    }

    response
        .result
        .ok_or_else(|| format!("Raw transaction not found: {tx_hash:?}").into())
}

/// Fetch execution witness using debug_executionWitness RPC.
async fn fetch_execution_witness(
    client: &reqwest::Client,
    rpc_url: &str,
    block_number: u64,
) -> Result<ExecutionWitnessResponse, Box<dyn std::error::Error>> {
    let response = client
        .post(rpc_url)
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "method": "debug_executionWitness",
            "params": [format!("0x{block_number:x}")],
            "id": 1
        }))
        .send()
        .await?
        .json::<RpcResponse<ExecutionWitnessResponse>>()
        .await?;

    if let Some(error) = response.error {
        return Err(format!(
            "debug_executionWitness error {}: {} (ensure the node supports this method)",
            error.code, error.message
        )
        .into());
    }

    response
        .result
        .ok_or_else(|| "Execution witness not found".into())
}

/// Fetch an L1 block by hash.
async fn fetch_l1_block(
    client: &reqwest::Client,
    rpc_url: &str,
    block_hash: B256,
) -> Result<Header, Box<dyn std::error::Error>> {
    let response = client
        .post(rpc_url)
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "method": "eth_getBlockByHash",
            "params": [format!("{block_hash:?}"), false],
            "id": 1
        }))
        .send()
        .await?
        .json::<RpcResponse<Header>>()
        .await?;

    if let Some(error) = response.error {
        return Err(format!("RPC error {}: {}", error.code, error.message).into());
    }

    response.result.ok_or_else(|| "L1 block not found".into())
}

/// Fetch L1 receipts for a block.
async fn fetch_l1_receipts(
    client: &reqwest::Client,
    rpc_url: &str,
    block_hash: B256,
) -> Result<Vec<ReceiptEnvelope>, Box<dyn std::error::Error>> {
    let response = client
        .post(rpc_url)
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "method": "eth_getBlockReceipts",
            "params": [format!("{block_hash:?}")],
            "id": 1
        }))
        .send()
        .await?
        .json::<RpcResponse<Vec<ReceiptEnvelope>>>()
        .await?;

    if let Some(error) = response.error {
        return Err(format!("RPC error {}: {}", error.code, error.message).into());
    }

    Ok(response.result.unwrap_or_default())
}

/// Fetch account proof using eth_getProof.
async fn fetch_account_proof(
    client: &reqwest::Client,
    rpc_url: &str,
    address: Address,
    block_number: u64,
) -> Result<AccountResult, Box<dyn std::error::Error>> {
    let response = client
        .post(rpc_url)
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "method": "eth_getProof",
            "params": [
                format!("{address:?}"),
                [],
                format!("0x{block_number:x}")
            ],
            "id": 1
        }))
        .send()
        .await?
        .json::<RpcResponse<AccountResult>>()
        .await?;

    if let Some(error) = response.error {
        return Err(format!("RPC error {}: {}", error.code, error.message).into());
    }

    response
        .result
        .ok_or_else(|| "Account proof not found".into())
}

/// Extract L1 origin hash from L1 info deposit tx in the block.
fn extract_l1_origin_hash(block: &L2BlockResponse) -> Result<B256, Box<dyn std::error::Error>> {
    let first_tx = block
        .transactions
        .first()
        .ok_or("Block has no transactions")?;

    let tx = OpTxEnvelope::decode_2718(&mut first_tx.as_ref())
        .map_err(|e| format!("Failed to decode deposit tx: {e}"))?;

    let deposit = match &tx {
        OpTxEnvelope::Deposit(d) => d,
        _ => return Err("First tx is not a deposit".into()),
    };

    let l1_info = L1BlockInfoTx::decode_calldata(deposit.input.as_ref())
        .map_err(|e| format!("Failed to decode L1BlockInfoTx: {e}"))?;

    Ok(l1_info.id().hash)
}

/// Extract sequenced transactions (all txs except the first deposit).
fn extract_sequenced_txs(block: &L2BlockResponse) -> Vec<Bytes> {
    // Skip the first transaction (L1 info deposit) and any other deposits
    block
        .transactions
        .iter()
        .skip(1) // Skip L1 info deposit
        .filter(|tx| {
            // Skip deposit transactions (type 0x7E = 126)
            tx.first().copied() != Some(0x7E)
        })
        .cloned()
        .collect()
}

/// Convert L2BlockResponse to Header.
fn block_response_to_header(block: &L2BlockResponse) -> Result<Header, Box<dyn std::error::Error>> {
    // Parse hex strings to u64
    let number = u64::from_str_radix(block.number.trim_start_matches("0x"), 16)?;
    let timestamp = u64::from_str_radix(block.timestamp.trim_start_matches("0x"), 16)?;

    Ok(Header {
        parent_hash: block.parent_hash,
        ommers_hash: alloy_primitives::b256!(
            "1dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0a142fd40d49347"
        ),
        beneficiary: block.beneficiary,
        state_root: block.state_root,
        transactions_root: block.transactions_root,
        receipts_root: block.receipts_root,
        logs_bloom: block.logs_bloom,
        difficulty: Default::default(),
        number,
        gas_limit: block.gas_limit,
        gas_used: block.gas_used,
        timestamp,
        extra_data: block.extra_data.clone(),
        mix_hash: block.mix_hash,
        nonce: Default::default(),
        base_fee_per_gas: block.base_fee_per_gas,
        withdrawals_root: block.withdrawals_root,
        blob_gas_used: None,
        excess_blob_gas: None,
        parent_beacon_block_root: block.parent_beacon_block_root,
        requests_hash: None,
    })
}

/// Get Base Sepolia rollup configuration.
fn get_base_sepolia_rollup_config() -> kona_genesis::RollupConfig {
    use alloy_eips::eip1898::BlockNumHash;
    use alloy_primitives::b256;
    use kona_genesis::{BaseFeeConfig, ChainGenesis, HardForkConfig, SystemConfig};

    kona_genesis::RollupConfig {
        l1_chain_id: 11155111,                            // Sepolia
        l2_chain_id: alloy_chains::Chain::from_id(84532), // Base Sepolia

        genesis: ChainGenesis {
            l1: BlockNumHash {
                number: 4370868,
                hash: b256!("cac9a83291d4dec146d6f7f69ab2304f23f5be87b1789119a0c5b1e4482444ed"),
            },
            l2: BlockNumHash {
                number: 0,
                hash: b256!("0dcc9e089e30b90ddfc55be9a37dd15bc551aeee999d2e2b51414c54eaf934e4"),
            },
            l2_time: 1695768288,
            system_config: Some(SystemConfig {
                batcher_address: "0x6CDEbe940BC0F26850285cacA097C11c33103E47"
                    .parse()
                    .unwrap(),
                gas_limit: 25_000_000,
                ..SystemConfig::default()
            }),
        },

        block_time: 2,
        max_sequencer_drift: 600,
        seq_window_size: 3600,
        channel_timeout: 300,
        granite_channel_timeout: 50,

        // Base Sepolia contract addresses
        deposit_contract_address: "0x49f53e41452C74589E85cA1677426Ba426459e85"
            .parse()
            .unwrap(),
        l1_system_config_address: "0xf272670eb55e895584501d564AfEB048bEd26194"
            .parse()
            .unwrap(),
        batch_inbox_address: "0xfF00000000000000000000000000000000084532"
            .parse()
            .unwrap(),
        protocol_versions_address: Address::ZERO,
        da_challenge_address: None,
        superchain_config_address: None,

        blobs_enabled_l1_timestamp: Some(0),

        // Base Sepolia hardfork timestamps
        hardforks: HardForkConfig {
            regolith_time: Some(0),
            canyon_time: Some(1699981200),
            delta_time: Some(1703203200),
            ecotone_time: Some(1708534800),
            fjord_time: Some(1716998400),
            granite_time: Some(1723478400),
            holocene_time: Some(1732633200),
            pectra_blob_schedule_time: Some(1742486400),
            isthmus_time: Some(1744905600),
            jovian_time: Some(1763568001),
            interop_time: None,
        },

        interop_message_expiry_window: 0,
        alt_da_config: None,
        chain_op_config: BaseFeeConfig {
            eip1559_elasticity: 10,
            eip1559_denominator: 50,
            eip1559_denominator_canyon: 250,
        },
    }
}

/// Get Sepolia L1 chain configuration.
fn get_sepolia_l1_config() -> L1ChainConfig {
    use alloy_eips::eip7840::BlobParams;
    use std::collections::BTreeMap;

    // Build the blob schedule with hardfork name -> BlobParams mapping
    let blob_schedule: BTreeMap<String, BlobParams> = BTreeMap::from([
        ("cancun".to_string(), BlobParams::cancun()),
        ("prague".to_string(), BlobParams::prague()),
        ("osaka".to_string(), BlobParams::osaka()),
        ("bpo1".to_string(), BlobParams::bpo1()),
        ("bpo2".to_string(), BlobParams::bpo2()),
    ]);

    // Sepolia L1 chain config with proper hardfork timestamps
    L1ChainConfig {
        chain_id: 11155111,
        // Sepolia hardfork timestamps
        homestead_block: Some(0),
        eip150_block: Some(0),
        eip155_block: Some(0),
        eip158_block: Some(0),
        byzantium_block: Some(0),
        constantinople_block: Some(0),
        petersburg_block: Some(0),
        istanbul_block: Some(0),
        berlin_block: Some(0),
        london_block: Some(0),
        // Merge (Paris) happened
        terminal_total_difficulty_passed: true,
        // Shanghai at 1677557088 (Mar 1, 2023)
        shanghai_time: Some(1677557088),
        // Cancun at 1706655072 (Jan 30, 2024)
        cancun_time: Some(1706655072),
        // Prague at 1741159200 (Mar 5, 2025)
        prague_time: Some(1741159200),
        // BPO hardfork timestamps for Sepolia
        // BPO1 at 1761017184 (approx June 2025)
        bpo1_time: Some(1761017184),
        // BPO2 at 1761607008 (approx June 2025)
        bpo2_time: Some(1761607008),
        // Blob schedule for correct blob base fee calculation
        blob_schedule,
        ..Default::default()
    }
}
