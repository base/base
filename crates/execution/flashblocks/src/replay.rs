//! Replay captured flashblocks against canonical receipts to pinpoint the first divergence.

use std::{
    collections::{BTreeMap, HashMap},
    fmt,
    fs::{self, File},
    future::Future,
    io::{BufRead, BufReader, BufWriter},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Instant,
};

use alloy_consensus::{
    Block, BlockBody, Eip658Value, Header, TxReceipt,
    transaction::{Recovered, SignerRecoverable},
};
use alloy_eips::{BlockId as AlloyBlockId, BlockNumberOrTag, Decodable2718};
use alloy_evm::block::BlockExecutor;
use alloy_network::{Ethereum, ReceiptResponse, TransactionResponse};
use alloy_primitives::{Address, B256, Bytes, U256, keccak256};
use alloy_provider::{Provider, ProviderBuilder};
use alloy_rpc_types::state::StateOverride;
use alloy_rpc_types_eth::Block as RpcBlock;
use base_common_consensus::{BaseBlock, BaseTxEnvelope};
use base_common_evm::BaseHaltReason;
use base_common_flashblocks::Flashblock;
use base_common_rpc_types::Transaction as RpcTransaction;
use base_execution_chainspec::BaseChainSpec;
use base_execution_evm::{BaseEvmConfig, BaseNextBlockEnvAttributes};
use futures::{StreamExt, TryStreamExt, future::try_join3, stream};
use reth_chainspec::{ChainInfo, ChainSpecProvider};
use reth_evm::{ConfigureEvm, execute::BlockBuilder};
use reth_primitives_traits::{Account, Bytecode, RecoveredBlock, SealedHeader};
use reth_provider::{
    AccountReader, BlockHashReader, BlockNumReader, HeaderProvider, ProviderError, ProviderResult,
    StateProofProvider, StateProvider, StateProviderFactory, StateRootProvider,
    StorageRootProvider,
};
use reth_trie_common::{
    AccountProof, HashedPostState, HashedStorage, MultiProof, MultiProofTargets, StorageMultiProof,
    StorageProof, TrieInput, updates::TrieUpdates,
};
use revm::context::result::ExecutionResult;
use revm::interpreter::{
    CallInputs, CallOutcome, CreateInputs, CreateOutcome, Interpreter,
    interpreter::EthInterpreter,
    interpreter_types::{Jumps, MemoryTr},
};
use revm::{bytecode::opcode::OpCode, inspector::Inspector};
use revm_database::{
    AlloyDB, BlockId, State, WrapDatabaseAsync, WrapDatabaseRef,
    states::bundle_state::BundleRetention,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::{Mutex as AsyncMutex, broadcast, mpsc};

use crate::{
    AssembledBlock, BlockAssembler, CanonicalBlockReconciler, PendingBlocks, PendingBlocksBuilder,
    PendingStateBuilder, ReconciliationStrategy, ReorgDetector, StateProcessor,
    StateProcessorError, StateUpdate, TransactionWithLogs,
};

type ReplayResult<T> = std::result::Result<T, ReplayError>;
type ReplayDb<P> = State<WrapDatabaseRef<WrapDatabaseAsync<AlloyDB<Ethereum, P>>>>;

/// Default pending depth used when mimicking the live state processor's reset behavior.
pub const DEFAULT_PROCESSOR_LIKE_MAX_DEPTH: u64 = 3;

/// The replay strategy to run against a captured block.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplayMode {
    /// Replay flashblocks and canonical commits using processor-style pending snapshots.
    ProcessorLike,
}

/// Event ordering variants to run through the live state processor.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplayEventScenario {
    /// Use the captured flashblock/canonical ordering as-is.
    Captured,
    /// Inject the parent canonical block after every flashblock.
    InjectParentCanonicalAfterFlashblock,
    /// Inject the current block's canonical block after every flashblock.
    InjectCurrentCanonicalAfterFlashblock,
    /// Inject both the parent and current canonical blocks after every flashblock.
    InjectParentAndCurrentCanonicalAfterFlashblock,
}

impl fmt::Display for ReplayEventScenario {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let label = match self {
            Self::Captured => "captured",
            Self::InjectParentCanonicalAfterFlashblock => {
                "inject_parent_canonical_after_flashblock"
            }
            Self::InjectCurrentCanonicalAfterFlashblock => {
                "inject_current_canonical_after_flashblock"
            }
            Self::InjectParentAndCurrentCanonicalAfterFlashblock => {
                "inject_parent_and_current_canonical_after_flashblock"
            }
        };
        f.write_str(label)
    }
}

impl fmt::Display for ReplayMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("processor_like")
    }
}

/// Inputs for a replay run.
#[derive(Debug, Clone)]
pub struct ReplayRequest {
    /// Directory containing the capture files.
    pub capture_dir: PathBuf,
    /// Archive-capable RPC endpoint used to fetch the canonical parent state.
    pub rpc_url: String,
    /// Optional first block to replay before the target block.
    pub start_block_number: Option<u64>,
    /// Canonical block number to replay.
    pub block_number: u64,
    /// Pending depth to use for processor-like replay.
    pub max_pending_blocks_depth: u64,
    /// Optional block-window sizes to replay, ending at `block_number`.
    pub window_block_counts: Vec<u64>,
    /// Event ordering scenarios to replay.
    pub event_scenarios: Vec<ReplayEventScenario>,
    /// Pending-depth values to replay.
    pub max_pending_blocks_depths: Vec<u64>,
    /// Maximum number of replay variants to run concurrently.
    pub parallelism: usize,
    /// Optional transaction hash to dump local and canonical traces for.
    pub trace_tx_hash: Option<B256>,
    /// Output directory for saved trace artifacts.
    pub trace_output_dir: Option<PathBuf>,
}

/// The result of replaying one mode for one captured block.
#[derive(Debug, Clone, Serialize)]
pub struct ReplaySummary {
    /// The replay strategy that produced this summary.
    pub mode: ReplayMode,
    /// The event ordering scenario used for this replay.
    pub event_scenario: ReplayEventScenario,
    /// The first canonical block included in the replay span.
    pub start_block_number: u64,
    /// The canonical block number that was replayed.
    pub block_number: u64,
    /// Number of blocks included in the replay span.
    pub window_block_count: u64,
    /// Pending depth configured for this replay.
    pub max_pending_blocks_depth: u64,
    /// Number of transactions replayed before completion or divergence.
    pub replayed_transactions: usize,
    /// The first divergence, if any.
    pub divergence: Option<ReplayDivergence>,
}

/// Details for the first replay divergence detected in a block.
#[derive(Debug, Clone, Serialize)]
pub struct ReplayDivergence {
    /// Canonical block number containing the divergence.
    pub block_number: u64,
    /// Canonical transaction index where replay diverged.
    pub tx_index: usize,
    /// Flashblock index containing the divergent transaction.
    pub flashblock_index: u64,
    /// Transaction hash at the divergence point.
    pub tx_hash: B256,
    /// Comparison data explaining the divergence.
    pub comparison: ReplayComparison,
}

/// The specific way a replay diverged from canonical execution.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReplayComparison {
    /// The replay produced a different transaction hash at the same position.
    TxHashMismatch {
        /// Hash produced locally by replay.
        local_tx_hash: B256,
        /// Hash recorded canonically at this position.
        canonical_tx_hash: B256,
    },
    /// The replay executed the same transaction but produced a different receipt outcome.
    OutcomeMismatch {
        /// Replay-derived transaction outcome.
        local: TxOutcome,
        /// Canonical transaction outcome.
        canonical: TxOutcome,
    },
    /// Local execution returned an error before matching the canonical result.
    ExecutionError {
        /// Stringified local execution error.
        error: String,
        /// Canonical transaction outcome for the same transaction.
        canonical: TxOutcome,
    },
    /// The replayed pending-RPC delta contained a different number of transactions.
    PendingRpcCountMismatch {
        /// Number of transactions produced locally for the flashblock delta.
        local_count: usize,
        /// Number of transactions captured from `newFlashblockTransactions(full=true)`.
        captured_count: usize,
    },
    /// The replayed pending-RPC delta emitted a different transaction hash.
    PendingRpcTxHashMismatch {
        /// Hash produced locally by replay.
        local_tx_hash: B256,
        /// Hash captured from the node's pending-RPC stream.
        captured_tx_hash: B256,
    },
    /// The replayed pending-RPC delta emitted a different status than the capture.
    PendingRpcOutcomeMismatch {
        /// Replay-derived pending-RPC outcome.
        local: TxOutcome,
        /// Pending-RPC outcome captured from the live node.
        captured: TxOutcome,
    },
    /// The replayed pending state and captured pending-RPC stream agree, but canonical differs.
    PendingCanonicalOutcomeMismatch {
        /// Replay-derived pending outcome.
        local: TxOutcome,
        /// Pending-RPC outcome captured from the live node.
        captured: TxOutcome,
        /// Canonical transaction outcome.
        canonical: TxOutcome,
    },
}

/// A compact transaction outcome used for local-versus-canonical comparisons.
#[derive(Debug, Clone, Eq, PartialEq, Serialize)]
pub struct TxOutcome {
    /// High-level status label: `success`, `revert`, or `halt`.
    pub status: &'static str,
    /// Gas used by the transaction.
    pub gas_used: u64,
    /// Number of logs emitted by the transaction.
    pub logs: usize,
}

/// A captured block assembled from the scratchpad monitor artifacts.
#[derive(Debug, Clone)]
pub struct CapturedBlock {
    /// Directory the capture was loaded from.
    pub capture_dir: PathBuf,
    /// Canonical block number represented by this capture.
    pub block_number: u64,
    /// Canonical block hash, if present in the capture.
    pub block_hash: Option<B256>,
    /// Flashblocks for the block in ascending index order.
    pub flashblocks: Vec<Flashblock>,
    /// Flattened transaction hash list reconstructed from the flashblocks.
    pub tx_hashes: Vec<B256>,
    /// Flashblock index for each flattened transaction.
    pub tx_flashblock_indices: Vec<u64>,
    /// Canonical receipts for the block in transaction order.
    pub canonical_receipts: Vec<CapturedCanonicalReceipt>,
}

/// Minimal canonical receipt data extracted from the capture files.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CapturedCanonicalReceipt {
    /// Canonical transaction hash.
    pub transaction_hash: B256,
    /// Whether canonical execution succeeded.
    #[serde(deserialize_with = "deserialize_receipt_status")]
    pub status: bool,
    /// Canonical gas used for the transaction.
    #[serde(deserialize_with = "deserialize_u64ish")]
    pub gas_used: u64,
    /// Number of logs emitted canonically.
    #[serde(default)]
    pub logs: Vec<serde_json::Value>,
}

/// Errors produced while loading or replaying a capture.
#[derive(Debug, Error)]
pub enum ReplayError {
    /// A capture file could not be read.
    #[error("failed to read {path}: {source}")]
    Io {
        /// Path that failed to read.
        path: PathBuf,
        #[source]
        /// Underlying I/O error.
        source: std::io::Error,
    },
    /// A capture file contained invalid JSON.
    #[error("failed to parse JSON in {path} at line {line}: {source}")]
    Json {
        /// File path containing the invalid JSON line.
        path: PathBuf,
        /// One-based line number that failed to parse.
        line: usize,
        #[source]
        /// Underlying JSON parsing error.
        source: serde_json::Error,
    },
    /// The capture did not contain any decoded flashblocks for the requested block.
    #[error("capture {capture_dir} does not contain flashblocks for block {block_number}")]
    MissingFlashblocks {
        /// Capture directory that was scanned.
        capture_dir: PathBuf,
        /// Requested canonical block number.
        block_number: u64,
    },
    /// The capture did not contain canonical receipts for the requested block.
    #[error("capture {capture_dir} does not contain canonical receipts for block {block_number}")]
    MissingCanonicalReceipts {
        /// Capture directory that was scanned.
        capture_dir: PathBuf,
        /// Requested canonical block number.
        block_number: u64,
    },
    /// The capture's flashblock tx count differed from the canonical receipt count.
    #[error(
        "captured flashblock tx count {flashblock_count} does not match canonical receipt count {canonical_count} for block {block_number}"
    )]
    TxCountMismatch {
        /// Block number with inconsistent transaction counts.
        block_number: u64,
        /// Transaction count reconstructed from flashblocks.
        flashblock_count: usize,
        /// Transaction count from canonical receipts.
        canonical_count: usize,
    },
    /// The capture's flattened tx hashes differed from canonical ordering.
    #[error(
        "captured tx hash mismatch at tx index {tx_index} for block {block_number}: flashblocks={flashblock_tx_hash} canonical={canonical_tx_hash}"
    )]
    CaptureTxHashMismatch {
        /// Block number with the mismatch.
        block_number: u64,
        /// Transaction index that mismatched.
        tx_index: usize,
        /// Transaction hash reconstructed from the flashblocks.
        flashblock_tx_hash: B256,
        /// Transaction hash recorded canonically at the same index.
        canonical_tx_hash: B256,
    },
    /// The captured canonical block JSON could not be converted into a recovered block.
    #[error("failed to decode canonical block {block_number}: {message}")]
    CanonicalBlockDecode {
        /// Block number that failed to decode.
        block_number: u64,
        /// Stringified decode error.
        message: String,
    },
    /// The captured pending-RPC stream could not be aligned to the captured flashblocks.
    #[error(
        "captured pending-rpc tx count {captured_count} does not match flashblock tx count {flashblock_count} for block {block_number}"
    )]
    PendingRpcTxCountMismatch {
        /// Block number with inconsistent transaction counts.
        block_number: u64,
        /// Transaction count captured from pending RPC.
        captured_count: usize,
        /// Transaction count reconstructed from flashblocks.
        flashblock_count: usize,
    },
    /// The captured pending-RPC ordering did not match the flashblock transaction ordering.
    #[error(
        "captured pending-rpc tx hash mismatch at tx index {tx_index} for block {block_number}: pending_rpc={pending_rpc_tx_hash} flashblocks={flashblock_tx_hash}"
    )]
    PendingRpcTxHashMismatch {
        /// Block number with the mismatch.
        block_number: u64,
        /// Transaction index that mismatched.
        tx_index: usize,
        /// Transaction hash captured from the pending-RPC stream.
        pending_rpc_tx_hash: B256,
        /// Transaction hash reconstructed from flashblocks.
        flashblock_tx_hash: B256,
    },
    /// The requested block had no parent block to use as a replay base.
    #[error("block {block_number} has no parent block to replay from")]
    MissingParentBlock {
        /// Block number that lacked a replayable parent.
        block_number: u64,
    },
    /// Archive RPC lookup failed while fetching parent-state data.
    #[error("failed to fetch parent header for block {block_number}: {message}")]
    Provider {
        /// Block number associated with the RPC failure.
        block_number: u64,
        /// Stringified provider error.
        message: String,
    },
    /// The current tokio runtime cannot be used to wrap the async archive database.
    #[error("tokio multi-thread runtime is required to wrap the archive RPC database")]
    TokioRuntimeUnavailable,
    /// A raw transaction in the capture could not be decoded.
    #[error("failed to decode tx at index {tx_index} in flashblock {flashblock_index}: {message}")]
    TransactionDecode {
        /// Transaction index within the flashblock.
        tx_index: usize,
        /// Flashblock index containing the bad transaction bytes.
        flashblock_index: u64,
        /// Stringified decode error.
        message: String,
    },
    /// The requested trace target was not present in the loaded captures.
    #[error("trace target transaction {tx_hash} was not found in the loaded captures")]
    TraceTargetNotFound {
        /// Transaction hash requested for tracing.
        tx_hash: B256,
    },
    /// Failed to serialize a generated trace artifact.
    #[error("failed to write JSON to {path}: {message}")]
    WriteJson {
        /// Output path that failed to serialize.
        path: PathBuf,
        /// Stringified serialization error.
        message: String,
    },
    /// The live flashblocks execution stack returned an error.
    #[error(transparent)]
    Flashblocks(#[from] StateProcessorError),
}

#[derive(Debug, Deserialize)]
struct FlashblockCaptureLine {
    #[serde(rename = "receivedAt")]
    received_at: String,
    #[serde(rename = "blockNumber", deserialize_with = "deserialize_u64ish")]
    block_number: u64,
    payload: Flashblock,
}

#[derive(Debug, Deserialize)]
struct CanonicalReceiptsCaptureLine {
    #[serde(rename = "blockNumber", deserialize_with = "deserialize_u64ish")]
    block_number: u64,
    #[serde(rename = "blockHash")]
    block_hash: B256,
    receipts: Vec<CapturedCanonicalReceipt>,
}

#[derive(Debug, Deserialize)]
struct CanonicalBlockCaptureLine {
    #[serde(rename = "receivedAt")]
    received_at: String,
    #[serde(rename = "blockNumber", deserialize_with = "deserialize_u64ish")]
    block_number: u64,
    #[serde(rename = "blockHash")]
    block_hash: B256,
    block: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct PendingRpcCaptureLine {
    #[serde(rename = "receivedAt")]
    received_at: String,
    result: TransactionWithLogs,
}

#[derive(Debug, Clone)]
enum ReplayEvent {
    Canonical { received_at: String, block: RecoveredBlock<BaseBlock> },
    Flashblock { received_at: String, flashblock: Flashblock },
}

#[derive(Debug, Clone, Copy)]
struct ReplayVariant {
    start_block_number: u64,
    window_block_count: u64,
    event_scenario: ReplayEventScenario,
    max_pending_blocks_depth: u64,
}

impl ReplayEvent {
    fn received_at(&self) -> &str {
        match self {
            Self::Canonical { received_at, .. } | Self::Flashblock { received_at, .. } => {
                received_at.as_str()
            }
        }
    }
}

/// Loads a single canonical block from a capture directory.
pub fn load_captured_block(
    capture_dir: impl AsRef<Path>,
    block_number: u64,
) -> ReplayResult<CapturedBlock> {
    let capture_dir = capture_dir.as_ref().to_path_buf();
    let flashblocks_path = capture_dir.join("flashblocks-decoded.ndjson");
    let canonical_receipts_path = capture_dir.join("canonical-receipts.ndjson");
    let canonical_blocks_path = capture_dir.join("canonical-blocks.ndjson");

    let mut flashblocks = read_ndjson::<FlashblockCaptureLine>(&flashblocks_path)?
        .into_iter()
        .filter(|entry| entry.block_number == block_number)
        .map(|entry| entry.payload)
        .collect::<Vec<_>>();
    flashblocks.sort_by_key(|flashblock| flashblock.index);

    if flashblocks.is_empty() {
        return Err(ReplayError::MissingFlashblocks { capture_dir, block_number });
    }

    let canonical_receipts_line =
        read_ndjson::<CanonicalReceiptsCaptureLine>(&canonical_receipts_path)?
            .into_iter()
            .find(|entry| entry.block_number == block_number)
            .ok_or_else(|| ReplayError::MissingCanonicalReceipts {
                capture_dir: capture_dir.clone(),
                block_number,
            })?;

    let block_hash = read_ndjson::<CanonicalBlockCaptureLine>(&canonical_blocks_path)
        .ok()
        .and_then(|entries| {
            entries
                .into_iter()
                .find(|entry| entry.block_number == block_number)
                .map(|entry| entry.block_hash)
        })
        .or(Some(canonical_receipts_line.block_hash));

    let mut tx_hashes = Vec::new();
    let mut tx_flashblock_indices = Vec::new();
    for flashblock in &flashblocks {
        for (tx_index, raw_tx) in flashblock.diff.transactions.iter().enumerate() {
            let tx = decode_transaction(raw_tx, flashblock.index, tx_index)?;
            tx_hashes.push(tx.tx_hash());
            tx_flashblock_indices.push(flashblock.index);
        }
    }

    if tx_hashes.len() != canonical_receipts_line.receipts.len() {
        return Err(ReplayError::TxCountMismatch {
            block_number,
            flashblock_count: tx_hashes.len(),
            canonical_count: canonical_receipts_line.receipts.len(),
        });
    }

    for (tx_index, (flashblock_tx_hash, canonical_receipt)) in
        tx_hashes.iter().zip(canonical_receipts_line.receipts.iter()).enumerate()
    {
        if *flashblock_tx_hash != canonical_receipt.transaction_hash {
            return Err(ReplayError::CaptureTxHashMismatch {
                block_number,
                tx_index,
                flashblock_tx_hash: *flashblock_tx_hash,
                canonical_tx_hash: canonical_receipt.transaction_hash,
            });
        }
    }

    Ok(CapturedBlock {
        capture_dir,
        block_number,
        block_hash,
        flashblocks,
        tx_hashes,
        tx_flashblock_indices,
        canonical_receipts: canonical_receipts_line.receipts,
    })
}

fn load_captured_blocks(
    capture_dir: impl AsRef<Path>,
    start_block_number: u64,
    end_block_number: u64,
) -> ReplayResult<Vec<CapturedBlock>> {
    (start_block_number..=end_block_number)
        .map(|block_number| load_captured_block(capture_dir.as_ref(), block_number))
        .collect()
}

/// Replays a captured block using the processor-like state machine.
pub async fn replay_capture(request: ReplayRequest) -> ReplayResult<Vec<ReplaySummary>> {
    let connect_start = Instant::now();
    let provider =
        ProviderBuilder::new().connect(request.rpc_url.as_str()).await.map_err(|error| {
            ReplayError::Provider {
                block_number: request.block_number.saturating_sub(1),
                message: error.to_string(),
            }
        })?;
    eprintln!("[replay] rpc connected in {:?}", connect_start.elapsed());

    let chain_spec = BaseChainSpec::mainnet();
    let shared_client = ReplayClient::new(
        provider.clone(),
        Arc::new(chain_spec.clone()),
        request.block_number,
        load_captured_block(&request.capture_dir, request.block_number)?
            .block_hash
            .unwrap_or_default(),
        std::iter::empty(),
    );
    let variants = replay_variants(&request);
    let parallelism = request.parallelism.max(1);
    let capture_dir = request.capture_dir.clone();
    let block_number = request.block_number;
    let trace_tx_hash = request.trace_tx_hash;
    let trace_output_dir = request.trace_output_dir.clone();

    let mut summaries = stream::iter(variants.into_iter().map(|variant| {
        let client = shared_client.clone();
        let capture_dir = capture_dir.clone();
        let trace_output_dir = trace_output_dir.clone();
        async move {
            replay_capture_variant(
                client,
                &capture_dir,
                block_number,
                variant,
                trace_tx_hash,
                trace_output_dir,
            )
            .await
        }
    }))
    .buffer_unordered(parallelism)
    .try_collect::<Vec<_>>()
    .await?;

    summaries.sort_by_key(|summary| {
        (summary.start_block_number, summary.max_pending_blocks_depth, summary.event_scenario)
    });
    Ok(summaries)
}

fn replay_variants(request: &ReplayRequest) -> Vec<ReplayVariant> {
    let window_block_counts = if request.window_block_counts.is_empty() {
        let start_block_number = request.start_block_number.unwrap_or(request.block_number);
        vec![request.block_number.saturating_sub(start_block_number).saturating_add(1)]
    } else {
        request.window_block_counts.clone()
    };
    let event_scenarios = if request.event_scenarios.is_empty() {
        vec![ReplayEventScenario::Captured]
    } else {
        request.event_scenarios.clone()
    };
    let max_pending_blocks_depths = if request.max_pending_blocks_depths.is_empty() {
        vec![request.max_pending_blocks_depth]
    } else {
        request.max_pending_blocks_depths.clone()
    };

    let mut variants = Vec::new();
    for window_block_count in window_block_counts {
        let start_block_number =
            request.block_number.saturating_sub(window_block_count.saturating_sub(1));
        for &event_scenario in &event_scenarios {
            for &max_pending_blocks_depth in &max_pending_blocks_depths {
                variants.push(ReplayVariant {
                    start_block_number,
                    window_block_count,
                    event_scenario,
                    max_pending_blocks_depth,
                });
            }
        }
    }
    variants
}

async fn replay_capture_variant<P>(
    client: ReplayClient<P>,
    capture_dir: &Path,
    block_number: u64,
    variant: ReplayVariant,
    trace_tx_hash: Option<B256>,
    trace_output_dir: Option<PathBuf>,
) -> ReplayResult<ReplaySummary>
where
    P: Provider<Ethereum> + Clone + fmt::Debug + Send + Sync + 'static,
{
    eprintln!(
        "[replay] variant scenario={} depth={} blocks={}..={} window={}",
        variant.event_scenario,
        variant.max_pending_blocks_depth,
        variant.start_block_number,
        block_number,
        variant.window_block_count,
    );
    let load_start = Instant::now();
    let captures = load_captured_blocks(capture_dir, variant.start_block_number, block_number)?;
    let pending_rpc_by_flashblock = load_pending_rpc_by_flashblock(
        capture_dir,
        &captures,
        variant.start_block_number,
        block_number,
    )?;
    let base_events = load_replay_events(capture_dir, variant.start_block_number, block_number)?;
    let events = replay_events_for_scenario(&base_events, variant.event_scenario);
    let total_transactions = captures.iter().map(|capture| capture.tx_hashes.len()).sum::<usize>();
    eprintln!(
        "[replay] variant loaded {} blocks, {} total txs, {} replay events in {:?}",
        captures.len(),
        total_transactions,
        events.len(),
        load_start.elapsed()
    );

    if let Some(trace_tx_hash) = trace_tx_hash {
        let trace_output_dir =
            trace_output_dir.unwrap_or_else(|| capture_dir.join("replay-traces"));
        let variant_trace_dir = trace_output_dir.join(format!(
            "start-{}-end-{}-depth-{}-scenario-{}",
            variant.start_block_number,
            block_number,
            variant.max_pending_blocks_depth,
            variant.event_scenario,
        ));
        dump_trace_artifacts(
            client.provider().clone(),
            client.chain_spec().as_ref(),
            &captures,
            &pending_rpc_by_flashblock,
            trace_tx_hash,
            variant.start_block_number,
            variant.max_pending_blocks_depth,
            &variant_trace_dir,
        )
        .await?;
    }

    replay_with_state_processor(client, &captures, &pending_rpc_by_flashblock, events, variant)
        .await
}

type PendingRpcByFlashblock = HashMap<(u64, u64), Vec<TransactionWithLogs>>;

fn replay_verbose() -> bool {
    std::env::var_os("BASE_FLASHBLOCKS_REPLAY_VERBOSE").is_some()
}

#[derive(Debug, Clone)]
struct ReplayClient<P> {
    inner: Arc<ReplayClientInner<P>>,
}

#[derive(Debug)]
struct ReplayClientInner<P> {
    provider: P,
    chain_spec: Arc<BaseChainSpec>,
    best_number: u64,
    best_hash: B256,
    headers_by_number: Mutex<HashMap<u64, Header>>,
    numbers_by_hash: Mutex<HashMap<B256, u64>>,
    accounts_by_block: Mutex<HashMap<(u64, Address), Option<Account>>>,
    storage_by_block: Mutex<HashMap<(u64, Address, B256), Option<U256>>>,
    bytecode_by_hash: Mutex<HashMap<B256, Bytecode>>,
}

#[derive(Debug, Clone)]
struct ReplayStateProvider<P> {
    client: ReplayClient<P>,
    block_number: u64,
}

impl<P> ReplayClient<P>
where
    P: Provider<Ethereum> + Clone + fmt::Debug + 'static,
{
    fn new(
        provider: P,
        chain_spec: Arc<BaseChainSpec>,
        best_number: u64,
        best_hash: B256,
        headers: impl IntoIterator<Item = Header>,
    ) -> Self {
        let mut headers_by_number = HashMap::new();
        let mut numbers_by_hash = HashMap::new();
        for header in headers {
            numbers_by_hash.insert(header.hash_slow(), header.number);
            headers_by_number.insert(header.number, header);
        }

        Self {
            inner: Arc::new(ReplayClientInner {
                provider,
                chain_spec,
                best_number,
                best_hash,
                headers_by_number: Mutex::new(headers_by_number),
                numbers_by_hash: Mutex::new(numbers_by_hash),
                accounts_by_block: Mutex::new(HashMap::new()),
                storage_by_block: Mutex::new(HashMap::new()),
                bytecode_by_hash: Mutex::new(HashMap::new()),
            }),
        }
    }

    fn provider(&self) -> &P {
        &self.inner.provider
    }

    fn block_on<T>(&self, future: impl Future<Output = ReplayResult<T>>) -> ProviderResult<T> {
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(future).map_err(ProviderError::other)
        })
    }

    fn cache_header(&self, header: Header) {
        let number = header.number;
        let hash = header.hash_slow();
        self.inner.headers_by_number.lock().unwrap().insert(number, header);
        self.inner.numbers_by_hash.lock().unwrap().insert(hash, number);
    }

    fn header_for_number(&self, number: u64) -> ProviderResult<Option<Header>> {
        if let Some(header) = self.inner.headers_by_number.lock().unwrap().get(&number).cloned() {
            return Ok(Some(header));
        }

        let provider = self.inner.provider.clone();
        let block = self.block_on(async move {
            provider
                .get_block_by_number(BlockNumberOrTag::Number(number))
                .await
                .map_err(|error| ReplayError::Provider {
                    block_number: number,
                    message: error.to_string(),
                })?
                .ok_or_else(|| ReplayError::Provider {
                    block_number: number,
                    message: "block not found on RPC".to_string(),
                })
        })?;
        let header = block.header.into_consensus();
        self.cache_header(header.clone());
        Ok(Some(header))
    }

    fn block_number_for_hash(&self, hash: B256) -> ProviderResult<Option<u64>> {
        if let Some(number) = self.inner.numbers_by_hash.lock().unwrap().get(&hash).copied() {
            return Ok(Some(number));
        }

        Ok(None)
    }
}

impl<P> ChainSpecProvider for ReplayClient<P>
where
    P: Provider<Ethereum> + Clone + fmt::Debug + Send + 'static,
{
    type ChainSpec = BaseChainSpec;

    fn chain_spec(&self) -> Arc<Self::ChainSpec> {
        Arc::clone(&self.inner.chain_spec)
    }
}

impl<P> BlockHashReader for ReplayClient<P>
where
    P: Provider<Ethereum> + Clone + fmt::Debug + Send + 'static,
{
    fn block_hash(&self, number: u64) -> ProviderResult<Option<B256>> {
        Ok(self.header_for_number(number)?.map(|header| header.hash_slow()))
    }

    fn canonical_hashes_range(&self, start: u64, end: u64) -> ProviderResult<Vec<B256>> {
        (start..end)
            .map(|number| self.block_hash(number).map(|hash| hash.unwrap_or_default()))
            .collect()
    }
}

impl<P> BlockNumReader for ReplayClient<P>
where
    P: Provider<Ethereum> + Clone + fmt::Debug + Send + 'static,
{
    fn chain_info(&self) -> ProviderResult<ChainInfo> {
        Ok(ChainInfo { best_hash: self.inner.best_hash, best_number: self.inner.best_number })
    }

    fn best_block_number(&self) -> ProviderResult<u64> {
        Ok(self.inner.best_number)
    }

    fn last_block_number(&self) -> ProviderResult<u64> {
        Ok(self.inner.best_number)
    }

    fn block_number(&self, hash: B256) -> ProviderResult<Option<u64>> {
        self.block_number_for_hash(hash)
    }
}

impl<P> reth_provider::BlockIdReader for ReplayClient<P>
where
    P: Provider<Ethereum> + Clone + fmt::Debug + Send + Sync + 'static,
{
    fn pending_block_num_hash(&self) -> ProviderResult<Option<alloy_eips::BlockNumHash>> {
        Ok(None)
    }

    fn safe_block_num_hash(&self) -> ProviderResult<Option<alloy_eips::BlockNumHash>> {
        Ok(None)
    }

    fn finalized_block_num_hash(&self) -> ProviderResult<Option<alloy_eips::BlockNumHash>> {
        Ok(None)
    }
}

impl<P> HeaderProvider for ReplayClient<P>
where
    P: Provider<Ethereum> + Clone + fmt::Debug + Send + 'static,
{
    type Header = Header;

    fn header(&self, block_hash: B256) -> ProviderResult<Option<Self::Header>> {
        self.block_number_for_hash(block_hash)?
            .map_or(Ok(None), |number| self.header_for_number(number))
    }

    fn header_by_number(&self, num: u64) -> ProviderResult<Option<Self::Header>> {
        self.header_for_number(num)
    }

    fn headers_range(
        &self,
        range: impl std::ops::RangeBounds<u64>,
    ) -> ProviderResult<Vec<Self::Header>> {
        use std::ops::Bound;

        let start = match range.start_bound() {
            Bound::Included(number) => *number,
            Bound::Excluded(number) => number.saturating_add(1),
            Bound::Unbounded => 0,
        };
        let end = match range.end_bound() {
            Bound::Included(number) => *number,
            Bound::Excluded(number) => number.saturating_sub(1),
            Bound::Unbounded => self.inner.best_number,
        };

        if start > end {
            return Ok(Vec::new());
        }

        (start..=end)
            .map(|number| self.header_for_number(number).map(|header| header.unwrap_or_default()))
            .collect()
    }

    fn sealed_header(&self, number: u64) -> ProviderResult<Option<SealedHeader<Header>>> {
        Ok(self.header_for_number(number)?.map(SealedHeader::seal_slow))
    }

    fn sealed_headers_while(
        &self,
        range: impl std::ops::RangeBounds<u64>,
        mut predicate: impl FnMut(&SealedHeader<Header>) -> bool,
    ) -> ProviderResult<Vec<SealedHeader<Header>>> {
        let mut headers = Vec::new();
        for header in self.headers_range(range)? {
            let sealed = SealedHeader::seal_slow(header);
            if !predicate(&sealed) {
                break;
            }
            headers.push(sealed);
        }
        Ok(headers)
    }
}

impl<P> StateProviderFactory for ReplayClient<P>
where
    P: Provider<Ethereum> + Clone + fmt::Debug + Send + Sync + 'static,
{
    fn latest(&self) -> ProviderResult<reth_provider::StateProviderBox> {
        self.history_by_block_number(self.inner.best_number)
    }

    fn state_by_block_number_or_tag(
        &self,
        number_or_tag: BlockNumberOrTag,
    ) -> ProviderResult<reth_provider::StateProviderBox> {
        match number_or_tag {
            BlockNumberOrTag::Number(number) => self.history_by_block_number(number),
            BlockNumberOrTag::Latest => self.latest(),
            BlockNumberOrTag::Earliest => self.history_by_block_number(0),
            BlockNumberOrTag::Pending => self.pending(),
            BlockNumberOrTag::Safe | BlockNumberOrTag::Finalized => self.latest(),
        }
    }

    fn history_by_block_number(
        &self,
        block: u64,
    ) -> ProviderResult<reth_provider::StateProviderBox> {
        Ok(Box::new(ReplayStateProvider { client: self.clone(), block_number: block }))
    }

    fn history_by_block_hash(
        &self,
        block: B256,
    ) -> ProviderResult<reth_provider::StateProviderBox> {
        let number =
            self.block_number_for_hash(block)?.ok_or(ProviderError::UnknownBlockHash(block))?;
        self.history_by_block_number(number)
    }

    fn state_by_block_hash(&self, block: B256) -> ProviderResult<reth_provider::StateProviderBox> {
        self.history_by_block_hash(block)
    }

    fn pending(&self) -> ProviderResult<reth_provider::StateProviderBox> {
        self.latest()
    }

    fn pending_state_by_hash(
        &self,
        block_hash: B256,
    ) -> ProviderResult<Option<reth_provider::StateProviderBox>> {
        self.state_by_block_hash(block_hash).map(Some)
    }

    fn maybe_pending(&self) -> ProviderResult<Option<reth_provider::StateProviderBox>> {
        self.pending().map(Some)
    }
}

impl<P> BlockHashReader for ReplayStateProvider<P>
where
    P: Provider<Ethereum> + Clone + fmt::Debug + Send + 'static,
{
    fn block_hash(&self, number: u64) -> ProviderResult<Option<B256>> {
        self.client.block_hash(number)
    }

    fn canonical_hashes_range(&self, start: u64, end: u64) -> ProviderResult<Vec<B256>> {
        self.client.canonical_hashes_range(start, end)
    }
}

impl<P> AccountReader for ReplayStateProvider<P>
where
    P: Provider<Ethereum> + Clone + fmt::Debug + Send + 'static,
{
    fn basic_account(&self, address: &Address) -> ProviderResult<Option<Account>> {
        if let Some(account) = self
            .client
            .inner
            .accounts_by_block
            .lock()
            .unwrap()
            .get(&(self.block_number, *address))
            .cloned()
        {
            return Ok(account);
        }

        let provider = self.client.provider().clone();
        let block_id = AlloyBlockId::Number(self.block_number.into());
        let address = *address;
        let (balance, nonce, code) = self.client.block_on(async move {
            try_join3(
                provider.get_balance(address).block_id(block_id).into_future(),
                provider.get_transaction_count(address).block_id(block_id).into_future(),
                provider.get_code_at(address).block_id(block_id).into_future(),
            )
            .await
            .map_err(|error| ReplayError::Provider {
                block_number: self.block_number,
                message: error.to_string(),
            })
        })?;

        if balance.is_zero() && nonce == 0 && code.is_empty() {
            self.client
                .inner
                .accounts_by_block
                .lock()
                .unwrap()
                .insert((self.block_number, address), None);
            return Ok(None);
        }

        let bytecode_hash = (!code.is_empty()).then(|| {
            let hash = keccak256(&code);
            self.client
                .inner
                .bytecode_by_hash
                .lock()
                .unwrap()
                .insert(hash, Bytecode::new_raw(code.clone()));
            hash
        });

        let account = Some(Account { balance, nonce, bytecode_hash });
        self.client
            .inner
            .accounts_by_block
            .lock()
            .unwrap()
            .insert((self.block_number, address), account.clone());
        Ok(account)
    }
}

impl<P> reth_provider::BytecodeReader for ReplayStateProvider<P>
where
    P: Provider<Ethereum> + Clone + fmt::Debug + Send + 'static,
{
    fn bytecode_by_hash(&self, code_hash: &B256) -> ProviderResult<Option<Bytecode>> {
        Ok(self.client.inner.bytecode_by_hash.lock().unwrap().get(code_hash).cloned())
    }
}

impl<P> StateRootProvider for ReplayStateProvider<P>
where
    P: Provider<Ethereum> + Clone + fmt::Debug + Send + 'static,
{
    fn state_root(&self, _state: HashedPostState) -> ProviderResult<B256> {
        Ok(B256::ZERO)
    }

    fn state_root_from_nodes(&self, _input: TrieInput) -> ProviderResult<B256> {
        Ok(B256::ZERO)
    }

    fn state_root_with_updates(
        &self,
        _state: HashedPostState,
    ) -> ProviderResult<(B256, TrieUpdates)> {
        Ok((B256::ZERO, TrieUpdates::default()))
    }

    fn state_root_from_nodes_with_updates(
        &self,
        _input: TrieInput,
    ) -> ProviderResult<(B256, TrieUpdates)> {
        Ok((B256::ZERO, TrieUpdates::default()))
    }
}

impl<P> StorageRootProvider for ReplayStateProvider<P>
where
    P: Provider<Ethereum> + Clone + fmt::Debug + Send + 'static,
{
    fn storage_root(
        &self,
        _address: Address,
        _hashed_storage: HashedStorage,
    ) -> ProviderResult<B256> {
        Ok(B256::ZERO)
    }

    fn storage_proof(
        &self,
        _address: Address,
        slot: B256,
        _hashed_storage: HashedStorage,
    ) -> ProviderResult<StorageProof> {
        Ok(StorageProof::new(slot))
    }

    fn storage_multiproof(
        &self,
        _address: Address,
        _slots: &[B256],
        _hashed_storage: HashedStorage,
    ) -> ProviderResult<StorageMultiProof> {
        Ok(StorageMultiProof::empty())
    }
}

impl<P> StateProofProvider for ReplayStateProvider<P>
where
    P: Provider<Ethereum> + Clone + fmt::Debug + Send + 'static,
{
    fn proof(
        &self,
        _input: TrieInput,
        address: Address,
        _slots: &[B256],
    ) -> ProviderResult<AccountProof> {
        Ok(AccountProof::new(address))
    }

    fn multiproof(
        &self,
        _input: TrieInput,
        _targets: MultiProofTargets,
    ) -> ProviderResult<MultiProof> {
        Ok(MultiProof::default())
    }

    fn witness(
        &self,
        _input: TrieInput,
        _target: HashedPostState,
        _mode: reth_trie_common::ExecutionWitnessMode,
    ) -> ProviderResult<Vec<Bytes>> {
        Ok(Vec::new())
    }
}

impl<P> reth_provider::HashedPostStateProvider for ReplayStateProvider<P>
where
    P: Provider<Ethereum> + Clone + fmt::Debug + Send + 'static,
{
    fn hashed_post_state(&self, _bundle_state: &revm_database::BundleState) -> HashedPostState {
        HashedPostState::default()
    }
}

impl<P> StateProvider for ReplayStateProvider<P>
where
    P: Provider<Ethereum> + Clone + fmt::Debug + Send + 'static,
{
    fn storage(&self, account: Address, storage_key: B256) -> ProviderResult<Option<U256>> {
        if let Some(value) = self
            .client
            .inner
            .storage_by_block
            .lock()
            .unwrap()
            .get(&(self.block_number, account, storage_key))
            .copied()
        {
            return Ok(value);
        }

        let provider = self.client.provider().clone();
        let block_id = AlloyBlockId::Number(self.block_number.into());
        let value = self.client.block_on(async move {
            provider
                .get_storage_at(account, U256::from_be_bytes(storage_key.0))
                .block_id(block_id)
                .await
                .map(Some)
                .map_err(|error| ReplayError::Provider {
                    block_number: self.block_number,
                    message: error.to_string(),
                })
        })?;
        self.client
            .inner
            .storage_by_block
            .lock()
            .unwrap()
            .insert((self.block_number, account, storage_key), value);
        Ok(value)
    }
}

struct LiveReplayState<P: Provider<Ethereum>> {
    db: ReplayDb<P>,
    state_overrides: StateOverride,
}

#[derive(Debug, Default)]
struct ProcessorStepOutcome {
    replayed_transactions: usize,
    divergence: Option<ReplayDivergence>,
}

impl ProcessorStepOutcome {
    const fn with_replayed(replayed_transactions: usize) -> Self {
        Self { replayed_transactions, divergence: None }
    }

    const fn with_divergence(replayed_transactions: usize, divergence: ReplayDivergence) -> Self {
        Self { replayed_transactions, divergence: Some(divergence) }
    }
}

struct RebuiltPendingState<P: Provider<Ethereum>> {
    pending_blocks: Arc<PendingBlocks>,
    live_state: LiveReplayState<P>,
}

struct ProcessorBuildOutcome<P: Provider<Ethereum>> {
    rebuilt_state: Option<RebuiltPendingState<P>>,
    step: ProcessorStepOutcome,
}

#[derive(Debug, Clone, Copy)]
struct TraceTarget {
    block_number: u64,
    flashblock_index: u64,
    tx_index: usize,
    offset_in_flashblock: usize,
    tx_hash: B256,
}

#[derive(Debug, Clone, Serialize)]
struct TraceArtifactsMetadata {
    start_block_number: u64,
    block_number: u64,
    flashblock_index: u64,
    tx_index: usize,
    offset_in_flashblock: usize,
    tx_hash: B256,
    captured_pending_outcome: Option<TxOutcome>,
    canonical_receipt: CapturedCanonicalReceipt,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum LocalTraceExecution {
    Executed { outcome: TxOutcome },
    Error { error: String },
}

#[derive(Debug, Clone, Serialize)]
struct LocalTraceArtifact {
    block_number: u64,
    flashblock_index: u64,
    tx_index: usize,
    offset_in_flashblock: usize,
    tx_hash: B256,
    execution: LocalTraceExecution,
    events: Vec<LocalTraceEvent>,
}

#[derive(Debug, Clone, Serialize)]
struct CanonicalBuilderComparisonArtifact {
    block_number: u64,
    tx_hash: B256,
    target_tx_index: usize,
    first_divergence: Option<CanonicalBuilderDivergence>,
}

#[derive(Debug, Clone, Serialize)]
struct CanonicalBuilderDivergence {
    tx_index: usize,
    tx_hash: B256,
    flashblocks: ComparableExecution,
    canonical_builder: ComparableExecution,
}

#[derive(Debug, Clone, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ComparableExecution {
    Executed { outcome: TxOutcome },
    Error { error: String },
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum LocalTraceEvent {
    Step {
        step: usize,
        pc: usize,
        opcode: String,
        contract: Address,
        bytecode_address: Option<Address>,
        caller: Address,
        stack_len: usize,
        memory_size: usize,
    },
    Call {
        depth: usize,
        caller: Address,
        target: Address,
        bytecode_address: Address,
        scheme: String,
        gas_limit: u64,
        value: U256,
        input_len: usize,
        is_static: bool,
    },
    CallEnd {
        depth: usize,
        status: String,
        output_len: usize,
    },
    Create {
        depth: usize,
        caller: Address,
        scheme: String,
        value: U256,
        gas_limit: u64,
        init_code_len: usize,
    },
    CreateEnd {
        depth: usize,
        status: String,
        address: Option<Address>,
        output_len: usize,
    },
    Log {
        depth: usize,
        address: Address,
        topics: Vec<B256>,
        data_len: usize,
    },
    Selfdestruct {
        depth: usize,
        contract: Address,
        target: Address,
        value: U256,
    },
}

trait TraceInspectorSnapshot {
    fn trace_events(&self) -> Vec<LocalTraceEvent>;
}

#[derive(Debug, Clone, Default, Serialize)]
struct LocalTraceInspector {
    #[serde(skip)]
    step_counter: usize,
    #[serde(skip)]
    call_depth: usize,
    events: Vec<LocalTraceEvent>,
}

impl TraceInspectorSnapshot for LocalTraceInspector {
    fn trace_events(&self) -> Vec<LocalTraceEvent> {
        self.events.clone()
    }
}

impl<CTX> Inspector<CTX, EthInterpreter> for LocalTraceInspector {
    fn step(&mut self, interp: &mut Interpreter<EthInterpreter>, _context: &mut CTX) {
        self.step_counter += 1;
        let opcode = interp.bytecode.opcode();
        let opcode = OpCode::new(opcode)
            .map(|value| value.to_string())
            .unwrap_or_else(|| format!("UNKNOWN_0x{opcode:02x}"));
        self.events.push(LocalTraceEvent::Step {
            step: self.step_counter,
            pc: interp.bytecode.pc(),
            opcode,
            contract: interp.input.target_address,
            bytecode_address: interp.input.bytecode_address,
            caller: interp.input.caller_address,
            stack_len: interp.stack.len(),
            memory_size: interp.memory.size(),
        });
    }

    fn call(&mut self, _context: &mut CTX, inputs: &mut CallInputs) -> Option<CallOutcome> {
        self.call_depth += 1;
        self.events.push(LocalTraceEvent::Call {
            depth: self.call_depth,
            caller: inputs.caller,
            target: inputs.target_address,
            bytecode_address: inputs.bytecode_address,
            scheme: format!("{:?}", inputs.scheme),
            gas_limit: inputs.gas_limit,
            value: inputs.call_value(),
            input_len: format!("{:?}", inputs.input).len(),
            is_static: inputs.is_static,
        });
        None
    }

    fn call_end(&mut self, _context: &mut CTX, _inputs: &CallInputs, outcome: &mut CallOutcome) {
        self.events.push(LocalTraceEvent::CallEnd {
            depth: self.call_depth,
            status: format!("{:?}", outcome.instruction_result()),
            output_len: outcome.output().len(),
        });
        self.call_depth = self.call_depth.saturating_sub(1);
    }

    fn create(&mut self, _context: &mut CTX, inputs: &mut CreateInputs) -> Option<CreateOutcome> {
        self.call_depth += 1;
        self.events.push(LocalTraceEvent::Create {
            depth: self.call_depth,
            caller: inputs.caller(),
            scheme: format!("{:?}", inputs.scheme()),
            value: inputs.value(),
            gas_limit: inputs.gas_limit(),
            init_code_len: inputs.init_code().len(),
        });
        None
    }

    fn create_end(
        &mut self,
        _context: &mut CTX,
        _inputs: &CreateInputs,
        outcome: &mut CreateOutcome,
    ) {
        self.events.push(LocalTraceEvent::CreateEnd {
            depth: self.call_depth,
            status: format!("{:?}", outcome.instruction_result()),
            address: outcome.address,
            output_len: outcome.output().len(),
        });
        self.call_depth = self.call_depth.saturating_sub(1);
    }

    fn log(&mut self, _context: &mut CTX, log: alloy_primitives::Log) {
        self.events.push(LocalTraceEvent::Log {
            depth: self.call_depth,
            address: log.address,
            topics: log.data.topics().to_vec(),
            data_len: log.data.data.len(),
        });
    }

    fn selfdestruct(&mut self, contract: Address, target: Address, value: U256) {
        self.events.push(LocalTraceEvent::Selfdestruct {
            depth: self.call_depth,
            contract,
            target,
            value,
        });
    }
}

async fn replay_processor_like<P>(
    provider: &P,
    chain_spec: &BaseChainSpec,
    captures: &[CapturedBlock],
    max_pending_blocks_depth: u64,
) -> ReplayResult<ReplaySummary>
where
    P: Provider<Ethereum> + Clone + fmt::Debug,
{
    let first_capture = captures
        .first()
        .ok_or(ReplayError::MissingFlashblocks { capture_dir: PathBuf::new(), block_number: 0 })?;
    let last_capture = captures
        .last()
        .ok_or(ReplayError::MissingFlashblocks { capture_dir: PathBuf::new(), block_number: 0 })?;

    let mut pending_blocks: Option<Arc<PendingBlocks>> = None;
    let mut live_state: Option<LiveReplayState<P>> = None;
    let mut replayed_transactions = 0usize;

    for (index, capture) in captures.iter().enumerate() {
        let block_start = Instant::now();
        eprintln!(
            "[replay] processor_like: block {} ({}/{}), {} flashblocks, {} txs",
            capture.block_number,
            index + 1,
            captures.len(),
            capture.flashblocks.len(),
            capture.tx_hashes.len()
        );

        for (flashblock_index, flashblock) in capture.flashblocks.iter().enumerate() {
            let flashblock_outcome = process_flashblock_processor_like(
                provider,
                chain_spec,
                captures,
                &mut pending_blocks,
                &mut live_state,
                flashblock,
            )
            .await?;
            replayed_transactions += flashblock_outcome.replayed_transactions;

            if let Some(divergence) = flashblock_outcome.divergence {
                return Ok(processor_like_summary(
                    first_capture.block_number,
                    last_capture
                        .block_number
                        .saturating_sub(first_capture.block_number)
                        .saturating_add(1),
                    max_pending_blocks_depth,
                    replayed_transactions,
                    divergence,
                ));
            }

            if let Some(previous_capture) = index
                .checked_sub(1)
                .and_then(|previous_index| captures.get(previous_index))
                .filter(|_| flashblock_index == 0)
            {
                let canonical_outcome = process_canonical_processor_like(
                    provider,
                    chain_spec,
                    captures,
                    &mut pending_blocks,
                    &mut live_state,
                    previous_capture.block_number,
                    max_pending_blocks_depth,
                )
                .await?;
                replayed_transactions += canonical_outcome.replayed_transactions;

                if let Some(divergence) = canonical_outcome.divergence {
                    return Ok(processor_like_summary(
                        first_capture.block_number,
                        last_capture
                            .block_number
                            .saturating_sub(first_capture.block_number)
                            .saturating_add(1),
                        max_pending_blocks_depth,
                        replayed_transactions,
                        divergence,
                    ));
                }
            }
        }

        eprintln!(
            "[replay] processor_like: finished block {} in {:?}",
            capture.block_number,
            block_start.elapsed()
        );
    }

    Ok(ReplaySummary {
        mode: ReplayMode::ProcessorLike,
        event_scenario: ReplayEventScenario::Captured,
        start_block_number: first_capture.block_number,
        block_number: last_capture.block_number,
        window_block_count: last_capture
            .block_number
            .saturating_sub(first_capture.block_number)
            .saturating_add(1),
        max_pending_blocks_depth,
        replayed_transactions,
        divergence: None,
    })
}

async fn process_flashblock_processor_like<P>(
    provider: &P,
    chain_spec: &BaseChainSpec,
    captures: &[CapturedBlock],
    pending_blocks: &mut Option<Arc<PendingBlocks>>,
    live_state: &mut Option<LiveReplayState<P>>,
    flashblock: &Flashblock,
) -> ReplayResult<ProcessorStepOutcome>
where
    P: Provider<Ethereum> + Clone + fmt::Debug,
{
    let Some(capture) =
        captures.iter().find(|capture| capture.block_number == flashblock.metadata.block_number)
    else {
        return Ok(ProcessorStepOutcome::default());
    };

    match pending_blocks.clone() {
        None => {
            if flashblock.index != 0 {
                return Ok(ProcessorStepOutcome::default());
            }

            let outcome = build_pending_state_partial_block(
                provider,
                chain_spec,
                capture,
                None,
                std::slice::from_ref(flashblock),
            )
            .await?;
            if let Some(rebuilt_state) = outcome.rebuilt_state {
                *pending_blocks = Some(rebuilt_state.pending_blocks);
                *live_state = Some(rebuilt_state.live_state);
            }
            Ok(outcome.step)
        }
        Some(previous_pending_blocks) => {
            if flashblock.metadata.block_number == previous_pending_blocks.latest_block_number() {
                process_same_block_flashblock(
                    chain_spec,
                    previous_pending_blocks,
                    live_state,
                    capture,
                    flashblock,
                    pending_blocks,
                )
            } else if flashblock.metadata.block_number
                == previous_pending_blocks.latest_block_number() + 1
                && flashblock.index == 0
            {
                process_next_block_flashblock(
                    chain_spec,
                    previous_pending_blocks,
                    live_state,
                    capture,
                    flashblock,
                    pending_blocks,
                )
            } else {
                *pending_blocks = None;
                *live_state = None;
                if flashblock.index != 0 {
                    return Ok(ProcessorStepOutcome::default());
                }

                let outcome = build_pending_state_partial_block(
                    provider,
                    chain_spec,
                    capture,
                    None,
                    std::slice::from_ref(flashblock),
                )
                .await?;
                if let Some(rebuilt_state) = outcome.rebuilt_state {
                    *pending_blocks = Some(rebuilt_state.pending_blocks);
                    *live_state = Some(rebuilt_state.live_state);
                }
                Ok(outcome.step)
            }
        }
    }
}

fn process_same_block_flashblock<P>(
    chain_spec: &BaseChainSpec,
    previous_pending_blocks: Arc<PendingBlocks>,
    live_state: &mut Option<LiveReplayState<P>>,
    capture: &CapturedBlock,
    flashblock: &Flashblock,
    pending_blocks: &mut Option<Arc<PendingBlocks>>,
) -> ReplayResult<ProcessorStepOutcome>
where
    P: Provider<Ethereum> + Clone + fmt::Debug,
{
    let Some(LiveReplayState { db, state_overrides }) = live_state.take() else {
        return Ok(ProcessorStepOutcome::default());
    };

    let latest_header = previous_pending_blocks.latest_header();
    let mut latest_block_flashblocks = previous_pending_blocks.latest_block_flashblocks();
    latest_block_flashblocks.push(flashblock.clone());
    let latest_block_header =
        BlockAssembler::refresh_same_block_header(&latest_header, &latest_block_flashblocks)?;

    let transactions = flashblock
        .diff
        .transactions
        .iter()
        .enumerate()
        .map(|(tx_index, raw_tx)| decode_transaction(raw_tx, flashblock.index, tx_index))
        .collect::<ReplayResult<Vec<_>>>()?;

    let pending_block = Block {
        header: Header {
            number: previous_pending_blocks.latest_block_base().block_number,
            timestamp: previous_pending_blocks.latest_block_base().timestamp,
            gas_limit: previous_pending_blocks.latest_block_base().gas_limit,
            base_fee_per_gas: Some(
                previous_pending_blocks.latest_block_base().base_fee_per_gas.saturating_to(),
            ),
            ..Default::default()
        },
        body: BlockBody { transactions: transactions.clone(), ..Default::default() },
    };

    let evm_config = BaseEvmConfig::base(Arc::new(chain_spec.clone()));
    let evm_env = evm_config.evm_env(&latest_header).map_err(|error| ReplayError::Provider {
        block_number: capture.block_number,
        message: error.to_string(),
    })?;
    let evm = evm_config.evm_with_env(db, evm_env);

    let mut pending_blocks_builder = PendingBlocksBuilder::from_previous(&previous_pending_blocks);
    pending_blocks_builder.with_flashblocks([flashblock.clone()]);
    pending_blocks_builder.replace_latest_header(latest_block_header);

    let mut pending_state_builder = PendingStateBuilder::new(
        chain_spec.clone(),
        evm,
        pending_block,
        None,
        previous_pending_blocks.latest_block_l1_block_info().clone(),
        state_overrides,
    );
    pending_state_builder.set_execution_offsets(
        previous_pending_blocks.latest_block_cumulative_gas_used(),
        previous_pending_blocks.latest_block_next_log_index(),
    );

    let step = execute_and_record_transactions(
        capture,
        &transactions,
        previous_pending_blocks.latest_block_transaction_count(),
        &mut pending_state_builder,
        &mut pending_blocks_builder,
    )?;
    if step.divergence.is_some() {
        return Ok(step);
    }

    let latest_block_transaction_count =
        previous_pending_blocks.latest_block_transaction_count() + transactions.len();
    let latest_block_cumulative_gas_used = pending_state_builder.cumulative_gas_used();
    let latest_block_next_log_index = pending_state_builder.next_log_index();
    let (mut db, state_overrides) = pending_state_builder.into_db_and_state_overrides();
    db.merge_transitions(BundleRetention::Reverts);
    pending_blocks_builder.with_bundle_state(db.bundle_state.clone());
    pending_blocks_builder.with_state_overrides(state_overrides.clone());
    pending_blocks_builder.with_latest_block_context(
        previous_pending_blocks.pending_transaction_count(),
        previous_pending_blocks.latest_block_base().clone(),
        previous_pending_blocks.latest_block_l1_block_info().clone(),
        latest_block_transaction_count,
        latest_block_cumulative_gas_used,
        latest_block_next_log_index,
    );

    let new_pending_blocks = Arc::new(pending_blocks_builder.build()?);
    *pending_blocks = Some(Arc::clone(&new_pending_blocks));
    *live_state = Some(LiveReplayState { db, state_overrides });
    Ok(step)
}

fn process_next_block_flashblock<P>(
    chain_spec: &BaseChainSpec,
    previous_pending_blocks: Arc<PendingBlocks>,
    live_state: &mut Option<LiveReplayState<P>>,
    capture: &CapturedBlock,
    flashblock: &Flashblock,
    pending_blocks: &mut Option<Arc<PendingBlocks>>,
) -> ReplayResult<ProcessorStepOutcome>
where
    P: Provider<Ethereum> + Clone + fmt::Debug,
{
    let Some(base) = flashblock.base.clone() else {
        return Ok(ProcessorStepOutcome::default());
    };
    let Some(LiveReplayState { db, state_overrides }) = live_state.take() else {
        return Ok(ProcessorStepOutcome::default());
    };

    let current_block = BlockAssembler::assemble(std::slice::from_ref(flashblock))?;
    let l1_block_info = current_block.l1_block_info()?;
    let AssembledBlock { block: assembled_block, header: assembled_header, .. } = current_block;
    let pending_block = Block {
        header: Header {
            number: base.block_number,
            timestamp: base.timestamp,
            gas_limit: base.gas_limit,
            base_fee_per_gas: Some(base.base_fee_per_gas.saturating_to()),
            ..Default::default()
        },
        body: assembled_block.body.clone(),
    };

    let evm_config = BaseEvmConfig::base(Arc::new(chain_spec.clone()));
    let evm_env = evm_config
        .next_evm_env(&previous_pending_blocks.latest_header(), &block_env_attributes(&base))
        .map_err(|error| ReplayError::Provider {
            block_number: capture.block_number,
            message: error.to_string(),
        })?;
    let evm = evm_config.evm_with_env(db, evm_env);

    let mut pending_blocks_builder = PendingBlocksBuilder::from_previous(&previous_pending_blocks);
    pending_blocks_builder.with_flashblocks([flashblock.clone()]);
    pending_blocks_builder.with_header(assembled_header);

    let mut pending_state_builder = PendingStateBuilder::new(
        chain_spec.clone(),
        evm,
        pending_block,
        None,
        l1_block_info.clone(),
        state_overrides,
    );
    pending_state_builder.apply_pre_execution_changes(
        previous_pending_blocks.latest_header().hash_slow(),
        Some(base.parent_beacon_block_root),
    )?;

    let step = execute_and_record_transactions(
        capture,
        &assembled_block.body.transactions,
        0,
        &mut pending_state_builder,
        &mut pending_blocks_builder,
    )?;
    if step.divergence.is_some() {
        return Ok(step);
    }

    let latest_block_cumulative_gas_used = pending_state_builder.cumulative_gas_used();
    let latest_block_next_log_index = pending_state_builder.next_log_index();
    let (mut db, state_overrides) = pending_state_builder.into_db_and_state_overrides();
    db.merge_transitions(BundleRetention::Reverts);
    pending_blocks_builder.with_bundle_state(db.bundle_state.clone());
    pending_blocks_builder.with_state_overrides(state_overrides.clone());
    pending_blocks_builder.with_latest_block_context(
        previous_pending_blocks.pending_transaction_count(),
        base,
        l1_block_info,
        flashblock.diff.transactions.len(),
        latest_block_cumulative_gas_used,
        latest_block_next_log_index,
    );

    let new_pending_blocks = Arc::new(pending_blocks_builder.build()?);
    *pending_blocks = Some(Arc::clone(&new_pending_blocks));
    *live_state = Some(LiveReplayState { db, state_overrides });
    Ok(step)
}

async fn process_canonical_processor_like<P>(
    provider: &P,
    chain_spec: &BaseChainSpec,
    captures: &[CapturedBlock],
    pending_blocks: &mut Option<Arc<PendingBlocks>>,
    live_state: &mut Option<LiveReplayState<P>>,
    canonical_block_number: u64,
    max_pending_blocks_depth: u64,
) -> ReplayResult<ProcessorStepOutcome>
where
    P: Provider<Ethereum> + Clone + fmt::Debug,
{
    let Some(previous_pending_blocks) = pending_blocks.clone() else {
        return Ok(ProcessorStepOutcome::default());
    };

    let tracked_txn_hashes: Vec<_> = previous_pending_blocks
        .get_transactions_for_block(canonical_block_number)
        .map(|transaction| transaction.tx_hash())
        .collect();
    let Some(canonical_capture) =
        captures.iter().find(|capture| capture.block_number == canonical_block_number)
    else {
        return Ok(ProcessorStepOutcome::default());
    };
    let reorg_detected =
        ReorgDetector::detect(&tracked_txn_hashes, &canonical_capture.tx_hashes).is_reorg();
    let strategy = CanonicalBlockReconciler::reconcile(
        Some(previous_pending_blocks.earliest_block_number()),
        Some(previous_pending_blocks.latest_block_number()),
        canonical_block_number,
        max_pending_blocks_depth,
        reorg_detected,
    );

    match strategy {
        ReconciliationStrategy::CatchUp | ReconciliationStrategy::NoPendingState => {
            *pending_blocks = None;
            *live_state = None;
            Ok(ProcessorStepOutcome::default())
        }
        ReconciliationStrategy::HandleReorg | ReconciliationStrategy::DepthLimitExceeded { .. } => {
            let future_flashblocks = previous_pending_blocks
                .get_flashblocks()
                .into_iter()
                .filter(|flashblock| flashblock.metadata.block_number > canonical_block_number)
                .collect::<Vec<_>>();

            if future_flashblocks.is_empty() {
                *pending_blocks = None;
                *live_state = None;
                return Ok(ProcessorStepOutcome::default());
            }

            let outcome = build_pending_state_window(
                provider,
                chain_spec,
                captures,
                None,
                &future_flashblocks,
            )
            .await?;
            if let Some(rebuilt_state) = outcome.rebuilt_state {
                *pending_blocks = Some(rebuilt_state.pending_blocks);
                *live_state = Some(rebuilt_state.live_state);
            }
            Ok(outcome.step)
        }
        ReconciliationStrategy::Continue => {
            let window_flashblocks = previous_pending_blocks.get_flashblocks();

            let outcome = build_pending_state_window(
                provider,
                chain_spec,
                captures,
                Some(previous_pending_blocks),
                &window_flashblocks,
            )
            .await?;
            if let Some(rebuilt_state) = outcome.rebuilt_state {
                *pending_blocks = Some(rebuilt_state.pending_blocks);
                *live_state = Some(rebuilt_state.live_state);
            }
            Ok(outcome.step)
        }
    }
}

async fn build_pending_state_window<P>(
    provider: &P,
    chain_spec: &BaseChainSpec,
    captures: &[CapturedBlock],
    previous_pending_blocks: Option<Arc<PendingBlocks>>,
    flashblocks: &[Flashblock],
) -> ReplayResult<ProcessorBuildOutcome<P>>
where
    P: Provider<Ethereum> + Clone + fmt::Debug,
{
    let mut flashblocks_per_block = BTreeMap::<u64, Vec<Flashblock>>::new();
    for flashblock in flashblocks {
        flashblocks_per_block
            .entry(flashblock.metadata.block_number)
            .or_default()
            .push(flashblock.clone());
    }

    let earliest_block_number = *flashblocks_per_block
        .keys()
        .next()
        .ok_or(ReplayError::MissingFlashblocks { capture_dir: PathBuf::new(), block_number: 0 })?;
    let canonical_block_number = earliest_block_number
        .checked_sub(1)
        .ok_or(ReplayError::MissingParentBlock { block_number: earliest_block_number })?;
    let mut last_block_header = fetch_parent_header(provider, canonical_block_number).await?;
    let mut db = make_db(provider.clone(), canonical_block_number)?;
    let mut state_overrides = previous_pending_blocks
        .as_ref()
        .and_then(|pending_blocks| pending_blocks.get_state_overrides())
        .unwrap_or_default();
    let mut pending_blocks_builder = PendingBlocksBuilder::new();
    let mut total_transaction_count = 0usize;
    let mut step = ProcessorStepOutcome::default();

    for (block_number, block_flashblocks) in flashblocks_per_block {
        let capture = captures
            .iter()
            .find(|capture| capture.block_number == block_number)
            .ok_or(ReplayError::MissingFlashblocks { capture_dir: PathBuf::new(), block_number })?;
        let assembled = BlockAssembler::assemble(&block_flashblocks)?;
        let latest_flashblock_tx_count = block_flashblocks
            .last()
            .map(|flashblock| flashblock.diff.transactions.len())
            .unwrap_or_default();
        let latest_block_base = assembled.base.clone();
        let latest_block_l1_block_info = assembled.l1_block_info()?;
        let latest_block_transaction_count = assembled.block.body.transactions.len();
        let parent_hash = last_block_header.hash_slow();
        let parent_beacon_block_root = Some(assembled.base.parent_beacon_block_root);

        pending_blocks_builder.with_flashblocks(assembled.flashblocks.clone());
        pending_blocks_builder.with_header(assembled.header.clone());

        let evm_config = BaseEvmConfig::base(Arc::new(chain_spec.clone()));
        let evm_env = evm_config
            .next_evm_env(&last_block_header, &block_env_attributes(&assembled.base))
            .map_err(|error| ReplayError::Provider { block_number, message: error.to_string() })?;
        let evm = evm_config.evm_with_env(db, evm_env);
        let mut pending_state_builder = PendingStateBuilder::new(
            chain_spec.clone(),
            evm,
            assembled.block.clone(),
            previous_pending_blocks.clone(),
            latest_block_l1_block_info.clone(),
            state_overrides,
        );
        pending_state_builder.apply_pre_execution_changes(parent_hash, parent_beacon_block_root)?;

        let block_step = execute_and_record_transactions(
            capture,
            &assembled.block.body.transactions,
            0,
            &mut pending_state_builder,
            &mut pending_blocks_builder,
        )?;
        step.replayed_transactions += block_step.replayed_transactions;
        if block_step.divergence.is_some() {
            step.divergence = block_step.divergence;
            return Ok(ProcessorBuildOutcome { rebuilt_state: None, step });
        }

        let latest_flashblock_tx_start = total_transaction_count
            .saturating_add(latest_block_transaction_count)
            .saturating_sub(latest_flashblock_tx_count);
        pending_blocks_builder.with_latest_block_context(
            latest_flashblock_tx_start,
            latest_block_base,
            latest_block_l1_block_info,
            latest_block_transaction_count,
            pending_state_builder.cumulative_gas_used(),
            pending_state_builder.next_log_index(),
        );
        total_transaction_count += latest_block_transaction_count;

        (db, state_overrides) = pending_state_builder.into_db_and_state_overrides();
        let (header, hash) = assembled.header.into_parts();
        last_block_header = SealedHeader::new(header, hash);
    }

    db.merge_transitions(BundleRetention::Reverts);
    pending_blocks_builder.with_bundle_state(db.bundle_state.clone());
    pending_blocks_builder.with_state_overrides(state_overrides.clone());
    let pending_blocks = Arc::new(pending_blocks_builder.build()?);

    Ok(ProcessorBuildOutcome {
        rebuilt_state: Some(RebuiltPendingState {
            pending_blocks,
            live_state: LiveReplayState { db, state_overrides },
        }),
        step,
    })
}

async fn build_pending_state_partial_block<P>(
    provider: &P,
    chain_spec: &BaseChainSpec,
    capture: &CapturedBlock,
    previous_pending_blocks: Option<Arc<PendingBlocks>>,
    flashblocks: &[Flashblock],
) -> ReplayResult<ProcessorBuildOutcome<P>>
where
    P: Provider<Ethereum> + Clone + fmt::Debug,
{
    let canonical_block_number = capture
        .block_number
        .checked_sub(1)
        .ok_or(ReplayError::MissingParentBlock { block_number: capture.block_number })?;
    let last_block_header = fetch_parent_header(provider, canonical_block_number).await?;
    let db = make_db(provider.clone(), canonical_block_number)?;
    let mut state_overrides = previous_pending_blocks
        .as_ref()
        .and_then(|pending_blocks| pending_blocks.get_state_overrides())
        .unwrap_or_default();
    let assembled = BlockAssembler::assemble(flashblocks)?;
    let latest_flashblock_tx_count =
        flashblocks.last().map(|flashblock| flashblock.diff.transactions.len()).unwrap_or_default();
    let latest_block_base = assembled.base.clone();
    let latest_block_l1_block_info = assembled.l1_block_info()?;
    let latest_block_transaction_count = assembled.block.body.transactions.len();

    let mut pending_blocks_builder = PendingBlocksBuilder::new();
    pending_blocks_builder.with_flashblocks(assembled.flashblocks.clone());
    pending_blocks_builder.with_header(assembled.header.clone());

    let evm_config = BaseEvmConfig::base(Arc::new(chain_spec.clone()));
    let evm_env = evm_config
        .next_evm_env(&last_block_header, &block_env_attributes(&assembled.base))
        .map_err(|error| ReplayError::Provider {
            block_number: capture.block_number,
            message: error.to_string(),
        })?;
    let evm = evm_config.evm_with_env(db, evm_env);
    let mut pending_state_builder = PendingStateBuilder::new(
        chain_spec.clone(),
        evm,
        assembled.block.clone(),
        previous_pending_blocks,
        latest_block_l1_block_info.clone(),
        state_overrides,
    );
    pending_state_builder.apply_pre_execution_changes(
        last_block_header.hash_slow(),
        Some(assembled.base.parent_beacon_block_root),
    )?;

    let step = execute_and_record_transactions(
        capture,
        &assembled.block.body.transactions,
        0,
        &mut pending_state_builder,
        &mut pending_blocks_builder,
    )?;
    if step.divergence.is_some() {
        return Ok(ProcessorBuildOutcome { rebuilt_state: None, step });
    }

    let latest_flashblock_tx_start =
        latest_block_transaction_count.saturating_sub(latest_flashblock_tx_count);
    let cumulative_gas_used = pending_state_builder.cumulative_gas_used();
    let next_log_index = pending_state_builder.next_log_index();
    let (mut db, state_overrides_after) = pending_state_builder.into_db_and_state_overrides();
    state_overrides = state_overrides_after.clone();
    db.merge_transitions(BundleRetention::Reverts);
    pending_blocks_builder.with_bundle_state(db.bundle_state.clone());
    pending_blocks_builder.with_state_overrides(state_overrides.clone());
    pending_blocks_builder.with_latest_block_context(
        latest_flashblock_tx_start,
        latest_block_base,
        latest_block_l1_block_info,
        latest_block_transaction_count,
        cumulative_gas_used,
        next_log_index,
    );

    let pending_blocks = Arc::new(pending_blocks_builder.build()?);
    Ok(ProcessorBuildOutcome {
        rebuilt_state: Some(RebuiltPendingState {
            pending_blocks,
            live_state: LiveReplayState { db, state_overrides },
        }),
        step,
    })
}

fn execute_and_record_transactions<E, DB>(
    capture: &CapturedBlock,
    transactions: &[BaseTxEnvelope],
    tx_index_offset: usize,
    pending_state_builder: &mut PendingStateBuilder<E, BaseChainSpec>,
    pending_blocks_builder: &mut PendingBlocksBuilder,
) -> ReplayResult<ProcessorStepOutcome>
where
    E: reth_evm::Evm<DB = DB, HaltReason = BaseHaltReason>,
    DB: revm::Database + revm::DatabaseCommit,
    E::Tx: reth_evm::FromRecoveredTx<BaseTxEnvelope>,
{
    let mut processed_transactions = 0usize;
    for (offset, transaction) in transactions.iter().cloned().enumerate() {
        let tx_index = tx_index_offset + offset;
        let canonical_receipt = &capture.canonical_receipts[tx_index];
        let expected_tx_hash = capture.tx_hashes[tx_index];
        let flashblock_index = flashblock_index_for_tx(capture, tx_index);
        let sender = transaction.recover_signer().map_err(StateProcessorError::from)?;
        let recovered_transaction = Recovered::new_unchecked(transaction, sender);
        let local_tx_hash = recovered_transaction.tx_hash();

        if local_tx_hash != expected_tx_hash {
            return Ok(ProcessorStepOutcome::with_divergence(
                processed_transactions + 1,
                ReplayDivergence {
                    block_number: capture.block_number,
                    tx_index,
                    flashblock_index,
                    tx_hash: expected_tx_hash,
                    comparison: ReplayComparison::TxHashMismatch {
                        local_tx_hash,
                        canonical_tx_hash: canonical_receipt.transaction_hash,
                    },
                },
            ));
        }

        match pending_state_builder.execute_transaction(tx_index, recovered_transaction) {
            Ok(executed_transaction) => {
                let local_success = execution_succeeded(&executed_transaction.result);
                if should_trace_tx(expected_tx_hash) {
                    eprintln!(
                        "[replay][processor_like] tx={} block={} index={} flashblock={} local_status={} canonical_status={} local_gas={} canonical_gas={}",
                        expected_tx_hash,
                        capture.block_number,
                        tx_index,
                        flashblock_index,
                        execution_status_label(&executed_transaction.result),
                        if canonical_receipt.status { "success" } else { "revert" },
                        executed_transaction.receipt.inner.gas_used(),
                        canonical_receipt.gas_used,
                    );
                }
                if local_success != canonical_receipt.status {
                    return Ok(ProcessorStepOutcome::with_divergence(
                        processed_transactions + 1,
                        ReplayDivergence {
                            block_number: capture.block_number,
                            tx_index,
                            flashblock_index,
                            tx_hash: expected_tx_hash,
                            comparison: ReplayComparison::OutcomeMismatch {
                                local: TxOutcome {
                                    status: execution_status_label(&executed_transaction.result),
                                    gas_used: executed_transaction.receipt.inner.gas_used(),
                                    logs: executed_transaction.receipt.inner.logs().len(),
                                },
                                canonical: TxOutcome {
                                    status: if canonical_receipt.status {
                                        "success"
                                    } else {
                                        "revert"
                                    },
                                    gas_used: canonical_receipt.gas_used,
                                    logs: canonical_receipt.logs.len(),
                                },
                            },
                        },
                    ));
                }

                pending_blocks_builder.with_transaction_sender(expected_tx_hash, sender);
                pending_blocks_builder.increment_nonce(sender);
                if let Some(time_us) = executed_transaction.execution_time_us {
                    pending_blocks_builder.with_execution_time(expected_tx_hash, time_us);
                }
                for (address, account) in &executed_transaction.state {
                    if account.is_touched() {
                        pending_blocks_builder.with_account_balance(*address, account.info.balance);
                    }
                }
                pending_blocks_builder.with_transaction(executed_transaction.rpc_transaction);
                pending_blocks_builder.with_receipt(expected_tx_hash, executed_transaction.receipt);
                pending_blocks_builder
                    .with_transaction_state(expected_tx_hash, executed_transaction.state);
                pending_blocks_builder
                    .with_transaction_result(expected_tx_hash, executed_transaction.result);
                processed_transactions += 1;
            }
            Err(error) => {
                return Ok(ProcessorStepOutcome::with_divergence(
                    processed_transactions + 1,
                    ReplayDivergence {
                        block_number: capture.block_number,
                        tx_index,
                        flashblock_index,
                        tx_hash: expected_tx_hash,
                        comparison: ReplayComparison::ExecutionError {
                            error: error.to_string(),
                            canonical: TxOutcome {
                                status: if canonical_receipt.status { "success" } else { "revert" },
                                gas_used: canonical_receipt.gas_used,
                                logs: canonical_receipt.logs.len(),
                            },
                        },
                    },
                ));
            }
        }
    }

    Ok(ProcessorStepOutcome::with_replayed(processed_transactions))
}

fn processor_like_summary(
    start_block_number: u64,
    window_block_count: u64,
    max_pending_blocks_depth: u64,
    replayed_transactions: usize,
    divergence: ReplayDivergence,
) -> ReplaySummary {
    ReplaySummary {
        mode: ReplayMode::ProcessorLike,
        event_scenario: ReplayEventScenario::Captured,
        start_block_number,
        block_number: divergence.block_number,
        window_block_count,
        max_pending_blocks_depth,
        replayed_transactions,
        divergence: Some(divergence),
    }
}

fn flashblock_index_for_tx(capture: &CapturedBlock, tx_index: usize) -> u64 {
    capture.tx_flashblock_indices.get(tx_index).copied().unwrap_or_default()
}

fn block_env_attributes(
    base: &base_common_flashblocks::ExecutionPayloadBaseV1,
) -> BaseNextBlockEnvAttributes {
    BaseNextBlockEnvAttributes {
        timestamp: base.timestamp,
        suggested_fee_recipient: base.fee_recipient,
        prev_randao: base.prev_randao,
        gas_limit: base.gas_limit,
        parent_beacon_block_root: Some(base.parent_beacon_block_root),
        extra_data: base.extra_data.clone(),
    }
}

fn execution_status_label(result: &ExecutionResult<BaseHaltReason>) -> &'static str {
    match result {
        ExecutionResult::Success { .. } => "success",
        ExecutionResult::Revert { .. } => "revert",
        ExecutionResult::Halt { .. } => "halt",
    }
}

fn execution_succeeded(result: &ExecutionResult<BaseHaltReason>) -> bool {
    matches!(result, ExecutionResult::Success { .. })
}

fn make_db<P>(
    provider: P,
    parent_number: u64,
) -> ReplayResult<State<WrapDatabaseRef<WrapDatabaseAsync<AlloyDB<Ethereum, P>>>>>
where
    P: Provider<Ethereum> + Clone + fmt::Debug,
{
    let alloy_db = AlloyDB::new(provider, BlockId::from(parent_number));
    let wrapped = WrapDatabaseAsync::new(alloy_db).ok_or(ReplayError::TokioRuntimeUnavailable)?;
    Ok(State::builder().with_database(WrapDatabaseRef(wrapped)).with_bundle_update().build())
}

async fn replay_with_state_processor<P>(
    client: ReplayClient<P>,
    captures: &[CapturedBlock],
    pending_rpc_by_flashblock: &PendingRpcByFlashblock,
    events: Vec<ReplayEvent>,
    variant: ReplayVariant,
) -> ReplayResult<ReplaySummary>
where
    P: Provider<Ethereum> + Clone + fmt::Debug + Send + Sync + 'static,
{
    let first_capture = captures
        .first()
        .ok_or(ReplayError::MissingFlashblocks { capture_dir: PathBuf::new(), block_number: 0 })?;
    let last_capture = captures
        .last()
        .ok_or(ReplayError::MissingFlashblocks { capture_dir: PathBuf::new(), block_number: 0 })?;

    let pending_blocks = Arc::new(arc_swap::ArcSwapOption::new(None));
    let (_tx, rx) = mpsc::unbounded_channel();
    let (sender, _) = broadcast::channel(16);
    let processor = StateProcessor::new(
        client,
        Arc::clone(&pending_blocks),
        variant.max_pending_blocks_depth,
        Arc::new(AsyncMutex::new(rx)),
        sender,
    );

    let mut replayed_transactions = 0usize;
    let total_events = events.len();
    let verbose = replay_verbose();
    for (event_index, event) in events.into_iter().enumerate() {
        let event_start = Instant::now();
        let pending_before = pending_blocks.load_full().map(|pending| {
            (
                pending.earliest_block_number(),
                pending.latest_block_number(),
                pending.latest_flashblock_index(),
            )
        });
        match event {
            ReplayEvent::Canonical { block, received_at } => {
                if verbose {
                    eprintln!(
                        "[replay] event {}/{} canonical block={} received_at={}",
                        event_index + 1,
                        total_events,
                        block.number,
                        received_at,
                    );
                }
                for published in
                    processor.process_update_for_replay(StateUpdate::Canonical(block)).await
                {
                    if let Some(divergence) = compare_pending_rpc_snapshot(
                        captures,
                        pending_rpc_by_flashblock,
                        &published,
                    ) {
                        return Ok(ReplaySummary {
                            mode: ReplayMode::ProcessorLike,
                            event_scenario: variant.event_scenario,
                            start_block_number: first_capture.block_number,
                            block_number: divergence.block_number,
                            window_block_count: variant.window_block_count,
                            max_pending_blocks_depth: variant.max_pending_blocks_depth,
                            replayed_transactions,
                            divergence: Some(divergence),
                        });
                    }
                }
            }
            ReplayEvent::Flashblock { flashblock, received_at } => {
                if verbose {
                    eprintln!(
                        "[replay] event {}/{} flashblock block={} index={} txs={} received_at={}",
                        event_index + 1,
                        total_events,
                        flashblock.metadata.block_number,
                        flashblock.index,
                        flashblock.diff.transactions.len(),
                        received_at,
                    );
                }
                replayed_transactions += flashblock.diff.transactions.len();
                for published in
                    processor.process_update_for_replay(StateUpdate::Flashblock(flashblock)).await
                {
                    if let Some(divergence) = compare_pending_rpc_snapshot(
                        captures,
                        pending_rpc_by_flashblock,
                        &published,
                    ) {
                        return Ok(ReplaySummary {
                            mode: ReplayMode::ProcessorLike,
                            event_scenario: variant.event_scenario,
                            start_block_number: first_capture.block_number,
                            block_number: divergence.block_number,
                            window_block_count: variant.window_block_count,
                            max_pending_blocks_depth: variant.max_pending_blocks_depth,
                            replayed_transactions,
                            divergence: Some(divergence),
                        });
                    }
                }
            }
        }
        let pending_after = pending_blocks.load_full().map(|pending| {
            (
                pending.earliest_block_number(),
                pending.latest_block_number(),
                pending.latest_flashblock_index(),
            )
        });
        if verbose {
            eprintln!(
                "[replay] event {}/{} finished in {:?} pending_before={:?} pending_after={:?}",
                event_index + 1,
                total_events,
                event_start.elapsed(),
                pending_before,
                pending_after,
            );
        }
    }

    Ok(ReplaySummary {
        mode: ReplayMode::ProcessorLike,
        event_scenario: variant.event_scenario,
        start_block_number: first_capture.block_number,
        block_number: last_capture.block_number,
        window_block_count: variant.window_block_count,
        max_pending_blocks_depth: variant.max_pending_blocks_depth,
        replayed_transactions,
        divergence: None,
    })
}

fn replay_events_for_scenario(
    base_events: &[ReplayEvent],
    scenario: ReplayEventScenario,
) -> Vec<ReplayEvent> {
    if matches!(scenario, ReplayEventScenario::Captured) {
        return base_events.to_vec();
    }

    let canonical_by_number = base_events
        .iter()
        .filter_map(|event| match event {
            ReplayEvent::Canonical { block, .. } => Some((block.number, block.clone())),
            ReplayEvent::Flashblock { .. } => None,
        })
        .collect::<HashMap<_, _>>();

    let mut events = Vec::with_capacity(base_events.len() * 3);
    for event in base_events {
        events.push(event.clone());
        let ReplayEvent::Flashblock { received_at, flashblock } = event else {
            continue;
        };
        let block_number = flashblock.metadata.block_number;

        let mut inject = |canonical_block_number: u64| {
            if let Some(block) = canonical_by_number.get(&canonical_block_number) {
                events.push(ReplayEvent::Canonical {
                    received_at: format!("{}+inject-{}", received_at, canonical_block_number),
                    block: block.clone(),
                });
            }
        };

        match scenario {
            ReplayEventScenario::Captured => {}
            ReplayEventScenario::InjectParentCanonicalAfterFlashblock => {
                inject(block_number.saturating_sub(1));
            }
            ReplayEventScenario::InjectCurrentCanonicalAfterFlashblock => {
                inject(block_number);
            }
            ReplayEventScenario::InjectParentAndCurrentCanonicalAfterFlashblock => {
                inject(block_number.saturating_sub(1));
                inject(block_number);
            }
        }
    }

    events
}

fn compare_pending_rpc_snapshot(
    captures: &[CapturedBlock],
    pending_rpc_by_flashblock: &PendingRpcByFlashblock,
    pending_blocks: &PendingBlocks,
) -> Option<ReplayDivergence> {
    let block_number = pending_blocks.latest_block_number();
    let flashblock_index = pending_blocks.latest_flashblock_index();
    let capture = captures.iter().find(|capture| capture.block_number == block_number)?;
    let local = pending_blocks.get_latest_flashblock_transactions_with_logs();
    let captured = pending_rpc_by_flashblock
        .get(&(block_number, flashblock_index))
        .cloned()
        .unwrap_or_default();
    let tx_index = capture
        .tx_flashblock_indices
        .iter()
        .position(|index| *index == flashblock_index)
        .unwrap_or_default();
    let tx_hash = capture.tx_hashes.get(tx_index).copied().unwrap_or_default();

    if local.len() != captured.len() {
        return Some(ReplayDivergence {
            block_number,
            tx_index,
            flashblock_index,
            tx_hash,
            comparison: ReplayComparison::PendingRpcCountMismatch {
                local_count: local.len(),
                captured_count: captured.len(),
            },
        });
    }

    for (offset, (local_tx, captured_tx)) in local.iter().zip(captured.iter()).enumerate() {
        let local_hash = local_tx.transaction.tx_hash();
        let captured_hash = captured_tx.transaction.tx_hash();
        let tx_index = tx_index + offset;
        let canonical_receipt = &capture.canonical_receipts[tx_index];

        if local_hash != captured_hash {
            return Some(ReplayDivergence {
                block_number,
                tx_index,
                flashblock_index,
                tx_hash: captured_hash,
                comparison: ReplayComparison::PendingRpcTxHashMismatch {
                    local_tx_hash: local_hash,
                    captured_tx_hash: captured_hash,
                },
            });
        }

        let local_outcome = pending_rpc_outcome(local_tx);
        let captured_outcome = pending_rpc_outcome(captured_tx);
        let canonical_outcome = TxOutcome {
            status: if canonical_receipt.status { "success" } else { "revert" },
            gas_used: canonical_receipt.gas_used,
            logs: canonical_receipt.logs.len(),
        };

        if local_outcome.status == captured_outcome.status
            && local_outcome.status != canonical_outcome.status
        {
            return Some(ReplayDivergence {
                block_number,
                tx_index,
                flashblock_index,
                tx_hash: captured_hash,
                comparison: ReplayComparison::PendingCanonicalOutcomeMismatch {
                    local: local_outcome,
                    captured: captured_outcome,
                    canonical: canonical_outcome,
                },
            });
        }

        if local_outcome.status != captured_outcome.status {
            return Some(ReplayDivergence {
                block_number,
                tx_index,
                flashblock_index,
                tx_hash: captured_hash,
                comparison: ReplayComparison::PendingRpcOutcomeMismatch {
                    local: local_outcome,
                    captured: captured_outcome,
                },
            });
        }
    }

    None
}

fn pending_rpc_outcome(transaction: &TransactionWithLogs) -> TxOutcome {
    TxOutcome {
        status: if transaction.status.coerce_status() { "success" } else { "revert" },
        gas_used: transaction.gas_used,
        logs: transaction.logs.len(),
    }
}

fn load_pending_rpc_by_flashblock(
    capture_dir: &Path,
    captures: &[CapturedBlock],
    start_block_number: u64,
    end_block_number: u64,
) -> ReplayResult<PendingRpcByFlashblock> {
    let path = capture_dir.join("rpc-new-flashblock-transactions.ndjson");
    let lines = read_ndjson::<PendingRpcCaptureLine>(&path)?;
    let mut by_block = HashMap::<u64, Vec<TransactionWithLogs>>::new();
    for line in lines {
        let block_number = line.result.transaction.block_number().unwrap_or_default();
        if !(start_block_number..=end_block_number).contains(&block_number) {
            continue;
        }
        by_block.entry(block_number).or_default().push(line.result);
    }

    let mut by_flashblock = HashMap::new();
    for capture in captures {
        let pending = by_block.remove(&capture.block_number).unwrap_or_default();
        if pending.len() != capture.tx_hashes.len() {
            return Err(ReplayError::PendingRpcTxCountMismatch {
                block_number: capture.block_number,
                captured_count: pending.len(),
                flashblock_count: capture.tx_hashes.len(),
            });
        }

        for (tx_index, (pending_tx, flashblock_tx_hash)) in
            pending.iter().zip(capture.tx_hashes.iter()).enumerate()
        {
            let pending_rpc_tx_hash = pending_tx.transaction.tx_hash();
            if pending_rpc_tx_hash != *flashblock_tx_hash {
                return Err(ReplayError::PendingRpcTxHashMismatch {
                    block_number: capture.block_number,
                    tx_index,
                    pending_rpc_tx_hash,
                    flashblock_tx_hash: *flashblock_tx_hash,
                });
            }
        }

        let mut cursor = 0usize;
        for flashblock in &capture.flashblocks {
            let tx_count = flashblock.diff.transactions.len();
            let end = cursor + tx_count;
            by_flashblock
                .insert((capture.block_number, flashblock.index), pending[cursor..end].to_vec());
            cursor = end;
        }
    }

    Ok(by_flashblock)
}

fn load_replay_events(
    capture_dir: &Path,
    start_block_number: u64,
    end_block_number: u64,
) -> ReplayResult<Vec<ReplayEvent>> {
    let flashblocks_path = capture_dir.join("flashblocks-decoded.ndjson");
    let canonical_blocks_path = capture_dir.join("canonical-blocks.ndjson");

    let mut events = read_ndjson::<FlashblockCaptureLine>(&flashblocks_path)?
        .into_iter()
        .filter(|entry| (start_block_number..=end_block_number).contains(&entry.block_number))
        .map(|entry| ReplayEvent::Flashblock {
            received_at: entry.received_at,
            flashblock: entry.payload,
        })
        .collect::<Vec<_>>();

    let mut canonical_events = read_ndjson::<CanonicalBlockCaptureLine>(&canonical_blocks_path)?
        .into_iter()
        .filter(|entry| {
            (start_block_number.saturating_sub(1)..=end_block_number).contains(&entry.block_number)
        })
        .map(|entry| {
            Ok(ReplayEvent::Canonical {
                received_at: entry.received_at,
                block: captured_rpc_block_to_recovered(entry.block_number, entry.block)?,
            })
        })
        .collect::<ReplayResult<Vec<_>>>()?;

    events.append(&mut canonical_events);
    events.sort_by(|left, right| left.received_at().cmp(right.received_at()));
    Ok(events)
}

fn captured_rpc_block_to_recovered(
    block_number: u64,
    block: serde_json::Value,
) -> ReplayResult<RecoveredBlock<BaseBlock>> {
    let block: RpcBlock<serde_json::Value> = serde_json::from_value(block).map_err(|error| {
        ReplayError::CanonicalBlockDecode { block_number, message: error.to_string() }
    })?;
    let header = block.header.into_consensus();
    let withdrawals = block.withdrawals;
    let transactions = block
        .transactions
        .try_into_transactions()
        .map_err(|_| ReplayError::CanonicalBlockDecode {
            block_number,
            message: "captured canonical block did not contain full transactions".to_string(),
        })?
        .into_iter()
        .map(|tx| decode_captured_rpc_transaction(block_number, tx))
        .collect::<ReplayResult<Vec<_>>>()?;
    let senders = transactions.iter().map(|tx: &RpcTransaction| tx.from()).collect::<Vec<_>>();
    let body = BlockBody {
        transactions: transactions.into_iter().map(|tx| tx.inner.into_inner()).collect(),
        ommers: vec![],
        withdrawals,
    };
    Ok(RecoveredBlock::new_unhashed(body.into_block(header), senders))
}

fn decode_captured_rpc_transaction(
    block_number: u64,
    mut transaction: serde_json::Value,
) -> ReplayResult<RpcTransaction> {
    if let Some(object) = transaction.as_object_mut() {
        if matches!(object.get("type"), Some(serde_json::Value::String(_)))
            && let Some(type_hex) = object.get("typeHex").cloned()
        {
            object.insert("type".to_string(), type_hex);
        }
    }

    serde_json::from_value(transaction).map_err(|error| ReplayError::CanonicalBlockDecode {
        block_number,
        message: error.to_string(),
    })
}

async fn fetch_parent_header<P>(
    provider: &P,
    parent_number: u64,
) -> ReplayResult<SealedHeader<Header>>
where
    P: Provider<Ethereum>,
{
    let block = provider
        .get_block_by_number(parent_number.into())
        .await
        .map_err(|error| ReplayError::Provider {
            block_number: parent_number,
            message: error.to_string(),
        })?
        .ok_or_else(|| ReplayError::Provider {
            block_number: parent_number,
            message: "block not found on RPC".to_string(),
        })?;

    Ok(SealedHeader::seal_slow(block.header.into_consensus()))
}

async fn dump_trace_artifacts<P>(
    provider: P,
    chain_spec: &BaseChainSpec,
    captures: &[CapturedBlock],
    pending_rpc_by_flashblock: &PendingRpcByFlashblock,
    tx_hash: B256,
    start_block_number: u64,
    max_pending_blocks_depth: u64,
    output_root: &Path,
) -> ReplayResult<()>
where
    P: Provider<Ethereum> + Clone + fmt::Debug + Send + Sync + 'static,
{
    let target = locate_trace_target(captures, tx_hash)
        .ok_or(ReplayError::TraceTargetNotFound { tx_hash })?;
    let capture = captures
        .iter()
        .find(|capture| capture.block_number == target.block_number)
        .ok_or(ReplayError::TraceTargetNotFound { tx_hash })?;
    let output_dir = output_root.join(tx_hash.to_string());
    fs::create_dir_all(&output_dir)
        .map_err(|source| ReplayError::Io { path: output_dir.clone(), source })?;

    eprintln!(
        "[replay] dumping traces for tx={} block={} flashblock={} tx_index={} into {}",
        tx_hash,
        target.block_number,
        target.flashblock_index,
        target.tx_index,
        output_dir.display(),
    );

    let metadata = TraceArtifactsMetadata {
        start_block_number,
        block_number: target.block_number,
        flashblock_index: target.flashblock_index,
        tx_index: target.tx_index,
        offset_in_flashblock: target.offset_in_flashblock,
        tx_hash,
        captured_pending_outcome: pending_rpc_by_flashblock
            .get(&(target.block_number, target.flashblock_index))
            .and_then(|transactions| transactions.get(target.offset_in_flashblock))
            .map(pending_rpc_outcome),
        canonical_receipt: capture.canonical_receipts[target.tx_index].clone(),
    };
    write_pretty_json(output_dir.join("trace-metadata.json"), &metadata)?;

    let struct_logs = fetch_canonical_rpc_trace(
        &provider,
        tx_hash,
        serde_json::json!({
            "disableStorage": true,
            "disableMemory": true,
            "enableReturnData": true,
        }),
    )
    .await?;
    write_pretty_json(output_dir.join("canonical-rpc-struct-logs.json"), &struct_logs)?;

    let call_trace = fetch_canonical_rpc_trace(
        &provider,
        tx_hash,
        serde_json::json!({
            "tracer": "callTracer",
            "tracerConfig": { "withLog": true },
        }),
    )
    .await?;
    write_pretty_json(output_dir.join("canonical-rpc-call-trace.json"), &call_trace)?;

    let local_trace = trace_transaction_locally(
        &provider,
        chain_spec,
        captures,
        target,
        max_pending_blocks_depth,
    )
    .await?;
    write_pretty_json(output_dir.join("local-inspector-trace.json"), &local_trace)?;

    let canonical_builder_comparison =
        compare_with_canonical_builder(&provider, chain_spec, capture, target).await?;
    write_pretty_json(
        output_dir.join("canonical-builder-comparison.json"),
        &canonical_builder_comparison,
    )?;

    Ok(())
}

async fn compare_with_canonical_builder<P>(
    provider: &P,
    chain_spec: &BaseChainSpec,
    capture: &CapturedBlock,
    target: TraceTarget,
) -> ReplayResult<CanonicalBuilderComparisonArtifact>
where
    P: Provider<Ethereum> + Clone + fmt::Debug,
{
    let canonical_block_number = capture
        .block_number
        .checked_sub(1)
        .ok_or(ReplayError::MissingParentBlock { block_number: capture.block_number })?;
    let parent_header = fetch_parent_header(provider, canonical_block_number).await?;
    let assembled = BlockAssembler::assemble(&capture.flashblocks)?;
    let l1_block_info = assembled.l1_block_info()?;
    let evm_config = BaseEvmConfig::base(Arc::new(chain_spec.clone()));

    let pending_db = make_db(provider.clone(), canonical_block_number)?;
    let pending_env = evm_config
        .next_evm_env(&parent_header, &block_env_attributes(&assembled.base))
        .map_err(|error| ReplayError::Provider {
            block_number: capture.block_number,
            message: error.to_string(),
        })?;
    let pending_evm = evm_config.evm_with_env(pending_db, pending_env);
    let mut pending_state_builder = PendingStateBuilder::new(
        chain_spec.clone(),
        pending_evm,
        assembled.block.clone(),
        None,
        l1_block_info,
        StateOverride::default(),
    );
    pending_state_builder.apply_pre_execution_changes(
        parent_header.hash(),
        Some(assembled.base.parent_beacon_block_root),
    )?;

    let mut canonical_db = make_db(provider.clone(), canonical_block_number)?;
    let mut canonical_builder = evm_config
        .builder_for_next_block(
            &mut canonical_db,
            &parent_header,
            block_env_attributes(&assembled.base),
        )
        .map_err(|error| ReplayError::Provider {
            block_number: capture.block_number,
            message: error.to_string(),
        })?;
    canonical_builder.apply_pre_execution_changes().map_err(|error| ReplayError::Provider {
        block_number: capture.block_number,
        message: error.to_string(),
    })?;

    let mut first_divergence = None;
    for (tx_index, transaction) in assembled.block.body.transactions.iter().cloned().enumerate() {
        let sender = transaction.recover_signer().map_err(StateProcessorError::from)?;
        let recovered = Recovered::new_unchecked(transaction, sender);
        let tx_hash = recovered.tx_hash();

        let flashblocks_execution =
            compare_flashblocks_execution(&mut pending_state_builder, tx_index, recovered.clone());
        let canonical_execution =
            compare_canonical_builder_execution(&mut canonical_builder, recovered);

        if first_divergence.is_none() && flashblocks_execution != canonical_execution {
            first_divergence = Some(CanonicalBuilderDivergence {
                tx_index,
                tx_hash,
                flashblocks: flashblocks_execution.clone(),
                canonical_builder: canonical_execution.clone(),
            });
        }

        if tx_hash == target.tx_hash {
            break;
        }
    }

    Ok(CanonicalBuilderComparisonArtifact {
        block_number: capture.block_number,
        tx_hash: target.tx_hash,
        target_tx_index: target.tx_index,
        first_divergence,
    })
}

fn compare_flashblocks_execution<E, DB>(
    pending_state_builder: &mut PendingStateBuilder<E, BaseChainSpec>,
    tx_index: usize,
    recovered: Recovered<BaseTxEnvelope>,
) -> ComparableExecution
where
    E: reth_evm::Evm<DB = DB, HaltReason = BaseHaltReason>,
    DB: revm::Database + revm::DatabaseCommit,
    E::Tx: reth_evm::FromRecoveredTx<BaseTxEnvelope>,
{
    match pending_state_builder.execute_transaction(tx_index, recovered) {
        Ok(executed_transaction) => ComparableExecution::Executed {
            outcome: TxOutcome {
                status: execution_status_label(&executed_transaction.result),
                gas_used: executed_transaction.receipt.inner.gas_used(),
                logs: executed_transaction.receipt.inner.logs().len(),
            },
        },
        Err(error) => ComparableExecution::Error { error: error.to_string() },
    }
}

fn compare_canonical_builder_execution<B>(
    canonical_builder: &mut B,
    recovered: Recovered<BaseTxEnvelope>,
) -> ComparableExecution
where
    B: BlockBuilder,
    B::Executor: BlockExecutor<Transaction = BaseTxEnvelope>,
    <B::Executor as BlockExecutor>::Receipt: TxReceipt,
{
    match canonical_builder.execute_transaction(recovered) {
        Ok(gas_used) => canonical_builder
            .executor()
            .receipts()
            .last()
            .map(|receipt| ComparableExecution::Executed {
                outcome: receipt_outcome(receipt, gas_used.tx_gas_used()),
            })
            .unwrap_or_else(|| ComparableExecution::Error {
                error: "canonical builder succeeded without recording a receipt".to_string(),
            }),
        Err(error) => ComparableExecution::Error { error: error.to_string() },
    }
}

fn receipt_outcome<R>(receipt: &R, gas_used: u64) -> TxOutcome
where
    R: TxReceipt,
{
    TxOutcome {
        status: receipt_status_label(receipt.status_or_post_state()),
        gas_used,
        logs: receipt.logs().len(),
    }
}

fn receipt_status_label(status: Eip658Value) -> &'static str {
    match status {
        Eip658Value::Eip658(true) => "success",
        Eip658Value::Eip658(false) => "revert",
        Eip658Value::PostState(_) => "post_state",
    }
}

fn locate_trace_target(captures: &[CapturedBlock], tx_hash: B256) -> Option<TraceTarget> {
    captures.iter().find_map(|capture| {
        capture.tx_hashes.iter().position(|candidate| *candidate == tx_hash).map(|tx_index| {
            let flashblock_index = capture.tx_flashblock_indices[tx_index];
            let first_in_flashblock = capture
                .tx_flashblock_indices
                .iter()
                .position(|index| *index == flashblock_index)
                .unwrap_or(tx_index);
            TraceTarget {
                block_number: capture.block_number,
                flashblock_index,
                tx_index,
                offset_in_flashblock: tx_index.saturating_sub(first_in_flashblock),
                tx_hash,
            }
        })
    })
}

async fn fetch_canonical_rpc_trace<P>(
    provider: &P,
    tx_hash: B256,
    options: serde_json::Value,
) -> ReplayResult<serde_json::Value>
where
    P: Provider<Ethereum>,
{
    provider
        .client()
        .request::<_, serde_json::Value>("debug_traceTransaction", (tx_hash, options))
        .await
        .map_err(|error| ReplayError::Provider { block_number: 0, message: error.to_string() })
}

async fn trace_transaction_locally<P>(
    provider: &P,
    chain_spec: &BaseChainSpec,
    captures: &[CapturedBlock],
    target: TraceTarget,
    max_pending_blocks_depth: u64,
) -> ReplayResult<LocalTraceArtifact>
where
    P: Provider<Ethereum> + Clone + fmt::Debug,
{
    let mut pending_blocks: Option<Arc<PendingBlocks>> = None;
    let mut live_state: Option<LiveReplayState<P>> = None;

    for (index, capture) in captures.iter().enumerate() {
        if capture.block_number > target.block_number {
            break;
        }

        for (flashblock_offset, flashblock) in capture.flashblocks.iter().enumerate() {
            if capture.block_number == target.block_number
                && flashblock.index == target.flashblock_index
            {
                return trace_flashblock_locally(
                    provider,
                    chain_spec,
                    captures,
                    &mut pending_blocks,
                    &mut live_state,
                    flashblock,
                    target,
                )
                .await;
            }

            let flashblock_outcome = process_flashblock_processor_like(
                provider,
                chain_spec,
                captures,
                &mut pending_blocks,
                &mut live_state,
                flashblock,
            )
            .await?;
            if flashblock_outcome.divergence.is_some() {
                return Err(ReplayError::TraceTargetNotFound { tx_hash: target.tx_hash });
            }

            if let Some(previous_capture) = index
                .checked_sub(1)
                .and_then(|previous_index| captures.get(previous_index))
                .filter(|_| flashblock_offset == 0)
            {
                let canonical_outcome = process_canonical_processor_like(
                    provider,
                    chain_spec,
                    captures,
                    &mut pending_blocks,
                    &mut live_state,
                    previous_capture.block_number,
                    max_pending_blocks_depth,
                )
                .await?;
                if canonical_outcome.divergence.is_some() {
                    return Err(ReplayError::TraceTargetNotFound { tx_hash: target.tx_hash });
                }
            }
        }
    }

    Err(ReplayError::TraceTargetNotFound { tx_hash: target.tx_hash })
}

async fn trace_flashblock_locally<P>(
    provider: &P,
    chain_spec: &BaseChainSpec,
    captures: &[CapturedBlock],
    pending_blocks: &mut Option<Arc<PendingBlocks>>,
    live_state: &mut Option<LiveReplayState<P>>,
    flashblock: &Flashblock,
    target: TraceTarget,
) -> ReplayResult<LocalTraceArtifact>
where
    P: Provider<Ethereum> + Clone + fmt::Debug,
{
    let capture = captures
        .iter()
        .find(|capture| capture.block_number == flashblock.metadata.block_number)
        .ok_or(ReplayError::TraceTargetNotFound { tx_hash: target.tx_hash })?;

    match pending_blocks.clone() {
        None => {
            trace_partial_block_transaction(provider, chain_spec, capture, flashblock, target).await
        }
        Some(previous_pending_blocks) => {
            if flashblock.metadata.block_number == previous_pending_blocks.latest_block_number() {
                trace_same_block_transaction(
                    chain_spec,
                    previous_pending_blocks,
                    live_state,
                    flashblock,
                    target,
                )
            } else {
                trace_next_block_transaction(
                    chain_spec,
                    previous_pending_blocks,
                    live_state,
                    flashblock,
                    target,
                )
            }
        }
    }
}

async fn trace_partial_block_transaction<P>(
    provider: &P,
    chain_spec: &BaseChainSpec,
    capture: &CapturedBlock,
    flashblock: &Flashblock,
    target: TraceTarget,
) -> ReplayResult<LocalTraceArtifact>
where
    P: Provider<Ethereum> + Clone + fmt::Debug,
{
    let canonical_block_number = capture
        .block_number
        .checked_sub(1)
        .ok_or(ReplayError::MissingParentBlock { block_number: capture.block_number })?;
    let last_block_header = fetch_parent_header(provider, canonical_block_number).await?;
    let db = make_db(provider.clone(), canonical_block_number)?;
    let assembled = BlockAssembler::assemble(std::slice::from_ref(flashblock))?;
    let latest_block_l1_block_info = assembled.l1_block_info()?;
    let evm_config = BaseEvmConfig::base(Arc::new(chain_spec.clone()));
    let evm_env = evm_config
        .next_evm_env(&last_block_header, &block_env_attributes(&assembled.base))
        .map_err(|error| ReplayError::Provider {
            block_number: capture.block_number,
            message: error.to_string(),
        })?;
    let evm = evm_config.evm_with_env_and_inspector(db, evm_env, LocalTraceInspector::default());
    let mut pending_state_builder = PendingStateBuilder::new(
        chain_spec.clone(),
        evm,
        assembled.block.clone(),
        None,
        latest_block_l1_block_info,
        StateOverride::default(),
    );
    pending_state_builder.apply_pre_execution_changes(
        last_block_header.hash_slow(),
        Some(assembled.base.parent_beacon_block_root),
    )?;
    trace_transaction_in_builder(
        &assembled.block.body.transactions,
        0,
        target,
        &mut pending_state_builder,
    )
}

fn trace_same_block_transaction<P>(
    chain_spec: &BaseChainSpec,
    previous_pending_blocks: Arc<PendingBlocks>,
    live_state: &mut Option<LiveReplayState<P>>,
    flashblock: &Flashblock,
    target: TraceTarget,
) -> ReplayResult<LocalTraceArtifact>
where
    P: Provider<Ethereum> + Clone + fmt::Debug,
{
    let Some(LiveReplayState { db, state_overrides }) = live_state.take() else {
        return Err(ReplayError::TraceTargetNotFound { tx_hash: target.tx_hash });
    };
    let transactions = flashblock
        .diff
        .transactions
        .iter()
        .enumerate()
        .map(|(tx_index, raw_tx)| decode_transaction(raw_tx, flashblock.index, tx_index))
        .collect::<ReplayResult<Vec<_>>>()?;
    let pending_block = Block {
        header: Header {
            number: previous_pending_blocks.latest_block_base().block_number,
            timestamp: previous_pending_blocks.latest_block_base().timestamp,
            gas_limit: previous_pending_blocks.latest_block_base().gas_limit,
            base_fee_per_gas: Some(
                previous_pending_blocks.latest_block_base().base_fee_per_gas.saturating_to(),
            ),
            ..Default::default()
        },
        body: BlockBody { transactions: transactions.clone(), ..Default::default() },
    };
    let evm_config = BaseEvmConfig::base(Arc::new(chain_spec.clone()));
    let evm_env =
        evm_config.evm_env(&previous_pending_blocks.latest_header()).map_err(|error| {
            ReplayError::Provider { block_number: target.block_number, message: error.to_string() }
        })?;
    let evm = evm_config.evm_with_env_and_inspector(db, evm_env, LocalTraceInspector::default());
    let mut pending_state_builder = PendingStateBuilder::new(
        chain_spec.clone(),
        evm,
        pending_block,
        None,
        previous_pending_blocks.latest_block_l1_block_info().clone(),
        state_overrides,
    );
    pending_state_builder.set_execution_offsets(
        previous_pending_blocks.latest_block_cumulative_gas_used(),
        previous_pending_blocks.latest_block_next_log_index(),
    );
    trace_transaction_in_builder(
        &transactions,
        previous_pending_blocks.latest_block_transaction_count(),
        target,
        &mut pending_state_builder,
    )
}

fn trace_next_block_transaction<P>(
    chain_spec: &BaseChainSpec,
    previous_pending_blocks: Arc<PendingBlocks>,
    live_state: &mut Option<LiveReplayState<P>>,
    flashblock: &Flashblock,
    target: TraceTarget,
) -> ReplayResult<LocalTraceArtifact>
where
    P: Provider<Ethereum> + Clone + fmt::Debug,
{
    let Some(base) = flashblock.base.clone() else {
        return Err(ReplayError::TraceTargetNotFound { tx_hash: target.tx_hash });
    };
    let Some(LiveReplayState { db, state_overrides }) = live_state.take() else {
        return Err(ReplayError::TraceTargetNotFound { tx_hash: target.tx_hash });
    };
    let current_block = BlockAssembler::assemble(std::slice::from_ref(flashblock))?;
    let l1_block_info = current_block.l1_block_info()?;
    let AssembledBlock { block: assembled_block, .. } = current_block;
    let evm_config = BaseEvmConfig::base(Arc::new(chain_spec.clone()));
    let evm_env = evm_config
        .next_evm_env(&previous_pending_blocks.latest_header(), &block_env_attributes(&base))
        .map_err(|error| ReplayError::Provider {
            block_number: target.block_number,
            message: error.to_string(),
        })?;
    let evm = evm_config.evm_with_env_and_inspector(db, evm_env, LocalTraceInspector::default());
    let mut pending_state_builder = PendingStateBuilder::new(
        chain_spec.clone(),
        evm,
        assembled_block.clone(),
        None,
        l1_block_info,
        state_overrides,
    );
    pending_state_builder.apply_pre_execution_changes(
        previous_pending_blocks.latest_header().hash_slow(),
        Some(base.parent_beacon_block_root),
    )?;
    trace_transaction_in_builder(
        &assembled_block.body.transactions,
        0,
        target,
        &mut pending_state_builder,
    )
}

fn trace_transaction_in_builder<E, DB>(
    transactions: &[BaseTxEnvelope],
    tx_index_offset: usize,
    target: TraceTarget,
    pending_state_builder: &mut PendingStateBuilder<E, BaseChainSpec>,
) -> ReplayResult<LocalTraceArtifact>
where
    E: reth_evm::Evm<DB = DB, HaltReason = BaseHaltReason>,
    DB: revm::Database + revm::DatabaseCommit,
    E::Tx: reth_evm::FromRecoveredTx<BaseTxEnvelope>,
    E::Inspector: TraceInspectorSnapshot,
{
    pending_state_builder.disable_inspector();
    for (offset, transaction) in
        transactions.iter().cloned().take(target.offset_in_flashblock).enumerate()
    {
        let sender = transaction.recover_signer().map_err(StateProcessorError::from)?;
        let recovered_transaction = Recovered::new_unchecked(transaction, sender);
        pending_state_builder
            .execute_transaction(tx_index_offset + offset, recovered_transaction)?;
    }

    let transaction = transactions
        .get(target.offset_in_flashblock)
        .cloned()
        .ok_or(ReplayError::TraceTargetNotFound { tx_hash: target.tx_hash })?;
    let sender = transaction.recover_signer().map_err(StateProcessorError::from)?;
    let recovered_transaction = Recovered::new_unchecked(transaction, sender);
    pending_state_builder.enable_inspector();
    let execution =
        match pending_state_builder.execute_transaction(target.tx_index, recovered_transaction) {
            Ok(executed_transaction) => LocalTraceExecution::Executed {
                outcome: TxOutcome {
                    status: execution_status_label(&executed_transaction.result),
                    gas_used: executed_transaction.receipt.inner.gas_used(),
                    logs: executed_transaction.receipt.inner.logs().len(),
                },
            },
            Err(error) => LocalTraceExecution::Error { error: error.to_string() },
        };
    Ok(LocalTraceArtifact {
        block_number: target.block_number,
        flashblock_index: target.flashblock_index,
        tx_index: target.tx_index,
        offset_in_flashblock: target.offset_in_flashblock,
        tx_hash: target.tx_hash,
        execution,
        events: pending_state_builder.inspector().trace_events(),
    })
}

fn write_pretty_json<T>(path: PathBuf, value: &T) -> ReplayResult<()>
where
    T: Serialize,
{
    let file =
        File::create(&path).map_err(|source| ReplayError::Io { path: path.clone(), source })?;
    serde_json::to_writer_pretty(BufWriter::new(file), value)
        .map_err(|error| ReplayError::WriteJson { path, message: error.to_string() })
}

fn read_ndjson<T>(path: &Path) -> ReplayResult<Vec<T>>
where
    T: for<'de> Deserialize<'de>,
{
    let file =
        File::open(path).map_err(|source| ReplayError::Io { path: path.to_path_buf(), source })?;
    let reader = BufReader::new(file);

    let mut entries = Vec::new();
    for (index, line) in reader.lines().enumerate() {
        let line = line.map_err(|source| ReplayError::Io { path: path.to_path_buf(), source })?;
        if line.trim().is_empty() {
            continue;
        }

        let entry = serde_json::from_str(&line).map_err(|source| ReplayError::Json {
            path: path.to_path_buf(),
            line: index + 1,
            source,
        })?;
        entries.push(entry);
    }

    Ok(entries)
}

fn decode_transaction(
    raw_tx: &Bytes,
    flashblock_index: u64,
    tx_index: usize,
) -> ReplayResult<BaseTxEnvelope> {
    BaseTxEnvelope::decode_2718_exact(raw_tx.as_ref()).map_err(|error| {
        ReplayError::TransactionDecode { tx_index, flashblock_index, message: error.to_string() }
    })
}

fn deserialize_u64ish<'de, D>(deserializer: D) -> std::result::Result<u64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    match value {
        serde_json::Value::Number(number) => number
            .as_u64()
            .ok_or_else(|| serde::de::Error::custom("expected u64-compatible number")),
        serde_json::Value::String(value) => {
            if let Some(value) = value.strip_prefix("0x") {
                u64::from_str_radix(value, 16)
                    .map_err(|error| serde::de::Error::custom(error.to_string()))
            } else {
                value.parse::<u64>().map_err(|error| serde::de::Error::custom(error.to_string()))
            }
        }
        _ => Err(serde::de::Error::custom("expected number or hex string")),
    }
}

fn deserialize_receipt_status<'de, D>(deserializer: D) -> std::result::Result<bool, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    match value {
        serde_json::Value::Bool(value) => Ok(value),
        serde_json::Value::String(value) => match value.as_str() {
            "success" => Ok(true),
            "revert" | "reverted" | "failure" | "failed" => Ok(false),
            "0x1" | "0x01" | "1" => Ok(true),
            "0x0" | "0x00" | "0" => Ok(false),
            _ => Err(serde::de::Error::custom("expected receipt status string or bool")),
        },
        serde_json::Value::Number(number) => number
            .as_u64()
            .map(|value| value != 0)
            .ok_or_else(|| serde::de::Error::custom("expected receipt status number")),
        _ => Err(serde::de::Error::custom("expected receipt status string, bool, or number")),
    }
}

fn should_trace_tx(tx_hash: B256) -> bool {
    let Ok(trace_hashes) = std::env::var("REPLAY_TRACE_TX") else {
        return false;
    };

    let tx_hash = tx_hash.to_string().to_ascii_lowercase();
    trace_hashes
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .any(|value| value.eq_ignore_ascii_case(&tx_hash))
}
