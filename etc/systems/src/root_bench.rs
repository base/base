//! Isolated serialized Engine build/import benchmark.

use std::{collections::BTreeMap, fs::{self, OpenOptions}, future::Future, io::{Read, Write}, panic::AssertUnwindSafe, path::{Path, PathBuf}, sync::{Arc, OnceLock}, time::{Duration, Instant}};
use alloy_consensus::{Transaction, TxEnvelope, TxReceipt, transaction::SignerRecoverable};
use alloy_eips::Decodable2718;
use alloy_genesis::Genesis;
use alloy_primitives::{Address, B64, B256, Bytes, U256, keccak256};
use alloy_provider::{Provider, RootProvider};
use alloy_rpc_client::RpcClient;
use alloy_rpc_types_engine::{ForkchoiceState, PayloadAttributes, PayloadStatus, PayloadStatusEnum};
use alloy_signer_local::PrivateKeySigner;
use base_common_consensus::JovianExtraData;
use base_common_network::{Base, BaseEngineApi, SharedEngineApi};
use base_common_rpc_types_engine::{BasePayloadAttributes, BaseExecutionPayloadV4, BaseExecutionPayloadEnvelopeV4, ExecutionData};
use base_execution_payload_builder::PostStateRootManager;
use base_consensus_engine::InMemoryTransport;
use base_execution_chainspec::BaseChainSpec;
use base_execution_cli::{ExecutionNodeArgs, ExecutionNodeLaunchConfig};
use base_load_tests::{Fees, PreparedTransaction, SubmissionPipeline, SubmitCohort};
use clap::{Args, Parser, Subcommand, ValueEnum};
use eyre::{Result, ensure, eyre};
use metrics_exporter_prometheus::PrometheusHandle;
use reth_chainspec::EthChainSpec;
use reth_cli_runner::CliContext;
use reth_provider::{AccountReader, ReceiptProvider, StateProviderFactory};
use reth_tasks::{PanickedTaskError, Runtime, RuntimeBuilder, RuntimeConfig};
use tokio::task::{JoinError, JoinHandle};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use futures::FutureExt;
use serde::ser::{SerializeMap, SerializeStruct};

/// Fixed synthetic gas limit shared by genesis, frozen attributes and validation.
pub const ROOT_BENCH_GAS_LIMIT: u64 = 126_000_000;

/// Fixed context for the production Engine driver and its mocked callers.
pub struct RootBenchDrive {
    /// Accepted fresh-genesis identity.
    pub genesis: B256,
    /// Initial blocks excluded from measurement.
    pub warmup: u64,
    /// Shared trial-relative clock origin.
    pub origin: Instant,
    /// Per-call deadline.
    pub timeout: Duration,
}

/// Borrowed JSONL view adds the full envelope without constructing a second JSON tree.
pub struct RootBenchSampleRecord<'a> {
    /// Retained sample, including typed payload bytes.
    pub sample: &'a RootBenchSample,
}

/// Borrowed response map keeps native envelopes in samples.jsonl only.
pub struct RootBenchResponses<'a> {
    /// Small serialized responses and the retained native envelope.
    pub sample: &'a RootBenchSample,
}

impl Serialize for RootBenchResponses<'_> {
    /// Streams the native payload directly, preserving the existing JSONL response shape.
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(None)?;
        for (name, value) in &self.sample.responses { if name != "payload" { map.serialize_entry(name, value)?; } }
        if let Some(envelope) = &self.sample.pending.envelope { map.serialize_entry("payload", envelope)?; }
        map.end()
    }
}

impl Serialize for RootBenchSampleRecord<'_> {
    /// Preserves every sample field while substituting a borrowed full-response map.
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        let sample = self.sample;
        let mut row = serializer.serialize_struct("RootBenchSample", 11)?;
        row.serialize_field("number", &sample.number)?;
        row.serialize_field("warmup", &sample.warmup)?;
        row.serialize_field("parent_hash", &sample.parent_hash)?;
        row.serialize_field("block_hash", &sample.block_hash)?;
        row.serialize_field("phases", &sample.phases)?;
        row.serialize_field("cycle_ns", &sample.cycle_ns)?;
        row.serialize_field("responses", &RootBenchResponses { sample })?;
        row.serialize_field("error", &sample.error)?;
        row.serialize_field("accepted_gas", &sample.accepted_gas)?;
        row.serialize_field("successful_receipts", &sample.successful_receipts)?;
        row.serialize_field("payload_reference", &sample.payload_reference)?;
        row.end()
    }
}

/// Root-benchmark process outcome, independent of diagnostic error text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootBenchExit {
    /// Refused before runtime execution.
    Refused,
    /// Active trial failed without an observed harness deadline.
    Incomplete,
    /// A harness deadline actually expired.
    Timeout,
}

impl std::fmt::Display for RootBenchExit {
    /// Formats the typed category without inferring it from internal worker messages.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { write!(f, "{self:?}") }
}
impl std::error::Error for RootBenchExit {}

/// Combined diagnostics preserve their primary category across evidence/cleanup failures.
#[derive(Debug)]
pub struct RootBenchFailure {
    /// Category used at the binary boundary.
    pub exit: RootBenchExit,
    /// Primary and subsequent diagnostics in observation order.
    pub message: String,
}
impl std::fmt::Display for RootBenchFailure {
    /// Retains the full diagnostic chain in human-readable errors.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str(&self.message) }
}
impl std::error::Error for RootBenchFailure {}

impl RootBenchExit {
    /// Reads only typed categories; opaque internal errors are incomplete, never inferred timeouts.
    pub fn category(error: &eyre::Report) -> Option<Self> {
        error.chain().find_map(|cause| cause.downcast_ref::<Self>().copied().or_else(|| cause.downcast_ref::<RootBenchFailure>().map(|e| e.exit)))
    }
    /// Maps only RootBench results to the documented process status.
    pub fn code(outcome: &Result<()>) -> i32 {
        match outcome { Ok(()) => 0, Err(error) => match Self::category(error).unwrap_or(Self::Incomplete) { Self::Refused => 2, Self::Incomplete => 3, Self::Timeout => 4 } }
    }
    /// Attaches a category while preserving all diagnostic text.
    pub fn attach(self, error: eyre::Report) -> eyre::Report {
        eyre::Report::new(RootBenchFailure { exit:Self::category(&error).unwrap_or(self), message:format!("{error:#}") })
    }
}

/// Minimal benchmark-local Engine calls, shared by real execution and default mock tests.
#[cfg_attr(test, mockall::automock)]
#[async_trait::async_trait]
pub trait RootBenchEngine: Send + Sync {
    /// Admits a build or canonically accepts a hash.
    async fn fcu(&self, state: ForkchoiceState, attrs: Option<BasePayloadAttributes>) -> Result<alloy_rpc_types_engine::ForkchoiceUpdated>;
    /// Waits for the native payload without an artificial delay.
    async fn payload(&self, id: alloy_rpc_types_engine::PayloadId) -> Result<BaseExecutionPayloadEnvelopeV4>;
    /// Imports the payload through independent validation.
    async fn import(&self, payload: BaseExecutionPayloadV4, beacon_root: B256) -> Result<PayloadStatus>;
}

#[async_trait::async_trait]
impl RootBenchEngine for SharedEngineApi {
    /// Uses the production FCU V3 handler directly.
    async fn fcu(&self, state: ForkchoiceState, attrs: Option<BasePayloadAttributes>) -> Result<alloy_rpc_types_engine::ForkchoiceUpdated> { Ok(self.fork_choice_updated_v3(state, attrs).await?) }
    /// Uses production getPayload V4 construction.
    async fn payload(&self, id: alloy_rpc_types_engine::PayloadId) -> Result<BaseExecutionPayloadEnvelopeV4> { Ok(self.get_payload_v4(id).await?) }
    /// Uses production newPayload V4 first import.
    async fn import(&self, payload: BaseExecutionPayloadV4, beacon_root: B256) -> Result<PayloadStatus> { Ok(self.new_payload_v4(payload, beacon_root).await?) }
}

/// Dedicated immutable-input benchmark commands.
#[derive(Debug, Parser)]
pub struct RootBenchArgs {
    /// Freeze inputs or run one isolated trial.
    #[command(subcommand)]
    pub command: RootBenchCommand,
}

/// Preparation is separate from measured execution.
#[derive(Debug, Subcommand)]
pub enum RootBenchCommand {
    /// Freeze signed transfers and fixed attributes once.
    Freeze {
        /// New numbered JSON path.
        #[arg(long)]
        output: PathBuf,
        /// Total blocks including warmup; at least four.
        #[arg(long, default_value_t = 8)]
        blocks: u64,
        /// Nonempty transfers per block.
        #[arg(long, default_value_t = 10)]
        transactions_per_block: u64,
        /// Number of initial blocks excluded from summaries.
        #[arg(long, default_value_t = 1)]
        warmup: u64,
    },
    /// Launch one admitted execution node and drive it without a CL.
    Run(RootBenchRunArgs),
}

/// One process must run only one trial, so metrics have no unrelated owners.
#[derive(Debug, Args)]
pub struct RootBenchRunArgs {
    /// Immutable frozen JSON.
    #[arg(long)]
    pub input: PathBuf,
    /// New numbered trial directory.
    #[arg(long)]
    pub output: PathBuf,
    /// Candidate cohort, not an immutable baseline.
    #[arg(long, value_enum)]
    pub cohort: RootBenchCohort,
    /// Externally attested executable-to-clean-source identity JSON, required before launch.
    #[arg(long)]
    pub executable_manifest: PathBuf,
    /// Deadline for each Engine/read call.
    #[arg(long, default_value_t = 30_000)]
    pub call_timeout_ms: u64,
    /// Deadline including launch and all blocks, excluding final drain.
    #[arg(long, default_value_t = 300_000)]
    pub run_timeout_ms: u64,
    /// Deadline for graceful root/persistence drain.
    #[arg(long, default_value_t = 30_000)]
    pub shutdown_timeout_ms: u64,
}

/// Matched candidate-source experiment arms.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ValueEnum)]
pub enum RootBenchCohort {
    /// Ordinary stock root options.
    O,
    /// Serial roots with trusted insertion.
    S,
    /// Serial roots with independent validator execution.
    R,
    /// Deferred parent commitment with independent validator execution.
    D,
}

/// Signed ordered workload with a canonical-encoding integrity digest.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RootBenchBundle {
    /// Format version.
    pub version: u64,
    /// Disposable custom chain only.
    pub chain_id: u64,
    /// Common genesis, without D's schedule.
    pub genesis: Genesis,
    /// Strictly post-genesis D activation.
    pub activation: u64,
    /// Warmup block count.
    pub warmup: u64,
    /// Fixed attributes, including signed transaction order.
    pub blocks: Vec<BasePayloadAttributes>,
    /// Keccak of the canonical serialization with digest zeroed.
    pub digest: B256,
}

/// Raw monotonic interval, relative to trial start, in nanoseconds.
#[derive(Debug, Clone, Serialize)]
pub struct RootBenchInterval {
    /// Request start.
    pub start_ns: u64,
    /// Response end.
    pub end_ns: u64,
}

/// Incrementally persisted phase evidence for one exact block hash.
#[derive(Debug, Clone, Serialize)]
pub struct RootBenchSample {
    /// Workload block index, one-based.
    pub number: u64,
    /// Excluded from summary numerators and percentiles.
    pub warmup: bool,
    /// Actual accepted cohort parent.
    pub parent_hash: B256,
    /// Native payload block hash, once available.
    pub block_hash: Option<B256>,
    /// Monotonic phase boundaries.
    pub phases: BTreeMap<String, RootBenchInterval>,
    /// Sum of exactly four Engine call durations; null until canonical acceptance.
    pub cycle_ns: Option<u64>,
    /// Unserialized responses from the current Engine sequence.
    #[serde(skip)]
    pub pending: RootBenchPending,
    /// Raw Engine responses and payload ID.
    pub responses: BTreeMap<String, Value>,
    /// JSONL row selector and JSON pointer for the full native payload, absent until available.
    pub payload_reference: Option<Value>,
    /// Failure retained separately from zero-duration success.
    pub error: Option<String>,
    /// Canonically accepted block gas.
    pub accepted_gas: u64,
    /// Successful canonical receipts.
    pub successful_receipts: u64,
}

/// Typed Engine evidence retained without serialization between successful calls.
#[derive(Debug, Default, Clone)]
pub struct RootBenchPending {
    /// Complete native payload retained for verification after saturation.
    pub envelope: Option<BaseExecutionPayloadEnvelopeV4>,
    /// Build admission and payload ID.
    pub build_fcu: Option<alloy_rpc_types_engine::ForkchoiceUpdated>,
    /// First import response.
    pub new_payload: Option<PayloadStatus>,
    /// Canonical acceptance response.
    pub canonical_fcu: Option<alloy_rpc_types_engine::ForkchoiceUpdated>,
    /// Reconstructed native header once available.
    pub payload_header: Option<alloy_consensus::Header>,
}

/// Exact nearest-rank nanosecond percentiles; absent for empty samples.
#[derive(Debug, Default, PartialEq, Serialize)]
pub struct RootBenchSummary {
    /// Number of successful measured observations.
    pub count: usize,
    /// Median.
    pub p50_ns: Option<u64>,
    /// 90th percentile.
    pub p90_ns: Option<u64>,
    /// 95th percentile.
    pub p95_ns: Option<u64>,
    /// 99th percentile.
    pub p99_ns: Option<u64>,
}

/// Owns trial shutdown, including provider work retained after cancellation.
pub struct RootBenchCleanup {
    /// Explicit execution runtime that must be shut down even after evidence failures.
    pub runtime: Runtime,
    /// Critical-task monitor, absent only when acquisition failed.
    pub critical: Option<JoinHandle<std::result::Result<(), PanickedTaskError>>>,
    /// Monitor result already consumed during the trial.
    pub critical_result: Option<std::result::Result<std::result::Result<(), PanickedTaskError>, JoinError>>,
    /// Shared worker manager observed before releasing the launched node.
    pub manager: Option<Arc<PostStateRootManager>>,
    /// Whether the cohort requires an observed worker manager.
    pub manager_required: bool,
    /// Blocking receipt and account checks, retained across all deadlines.
    pub receipt_tasks: Vec<JoinHandle<Result<u64>>>,
    /// Bound for each shutdown phase.
    pub timeout: Duration,
}

impl RootBenchCleanup {
    /// Keeps the first failure and appends later cleanup or evidence failures.
    pub fn retain_error(outcome: &mut Result<()>, result: Result<()>) {
        if let Err(error) = result {
            *outcome = Err(match outcome.as_ref() {
                Ok(()) => error,
                Err(primary) => eyre::Report::new(RootBenchFailure {
                    exit:if RootBenchExit::category(primary) == Some(RootBenchExit::Timeout) || RootBenchExit::category(&error) == Some(RootBenchExit::Timeout) { RootBenchExit::Timeout } else { RootBenchExit::category(primary).unwrap_or(RootBenchExit::Incomplete) },
                    message:format!("{primary:#}; subsequent failure: {error:#}"),
                }),
            });
        }
    }

    /// Attempts graceful drain, checks actual workers/reads, then always shuts down the runtime.
    pub async fn finish(mut self, last: Option<&mut RootBenchSample>, output: &Path, outcome: &mut Result<()>) -> Value {
        if let Err(error) = &outcome {
            if let Some(last) = last {
                last.error.get_or_insert_with(|| format!("{error:#}"));
                Self::retain_error(outcome, last.flush_pending());
                Self::retain_error(outcome, last.persist(output));
            }
        }
        let drain_runtime = self.runtime.clone();
        let timeout = self.timeout;
        let (guards_completed, deadline_observed) = match tokio::task::spawn_blocking(move || drain_runtime.graceful_shutdown_with_timeout(timeout)).await {
            Ok(completed) => (completed, !completed),
            Err(error) => { Self::retain_error(outcome, Err(eyre!("graceful shutdown join failed: {error}"))); (false, false) }
        };
        if deadline_observed { Self::retain_error(outcome, Err(RootBenchExit::Timeout.attach(eyre!("shutdown/drain deadline exceeded; trial incomplete")))); }
        // The task manager can exit before shutdown-task panics. Worker state is required too.
        if self.critical_result.is_none() {
            if let Some(critical) = self.critical.as_mut() {
                match tokio::time::timeout(timeout, critical).await {
                    Ok(result) => self.critical_result = Some(result),
                    Err(_) => Self::retain_error(outcome, Err(RootBenchExit::Timeout.attach(eyre!("critical-task monitor shutdown deadline exceeded")))),
                }
            } else { Self::retain_error(outcome, Err(eyre!("runtime critical-task monitor unavailable"))); }
        }
        let worker_result = RootBenchRunArgs::verify_worker(self.manager.as_ref(), self.manager_required);
        let worker_joined_healthy = worker_result.is_ok();
        Self::retain_error(outcome, worker_result);
        let receipt_reads_completed = self.receipt_tasks.iter().all(|task| task.is_finished());
        for task in &mut self.receipt_tasks {
            if task.is_finished() {
                let result = task.await.map_err(|error| eyre!("provider read join failed: {error}")).and_then(|result| result.map(|_| ()));
                Self::retain_error(outcome, result);
            }
        }
        if !receipt_reads_completed { Self::retain_error(outcome, Err(eyre!("receipt/account provider work remains outstanding after shutdown"))); }
        if matches!(&self.critical_result, Some(Err(_)) | Some(Ok(Err(_)))) { Self::retain_error(outcome, Err(eyre!("critical task failure: {:?}", self.critical_result))); }
        let drained = guards_completed && worker_joined_healthy && receipt_reads_completed;
        let shutdown = tokio::task::spawn_blocking(move || self.runtime.shutdown_timeout(timeout)).await.map_err(|error| eyre!("runtime shutdown join failed: {error}"));
        Self::retain_error(outcome, shutdown);
        json!({"schema_version":1,"drained":drained,"guards_completed":guards_completed,"worker_joined_healthy":worker_joined_healthy,"receipt_reads_completed":receipt_reads_completed,"manager_observed":self.manager.is_some(),"critical_task_result":format!("{:?}", self.critical_result)})
    }
}

impl RootBenchArgs {
    /// Dispatches preparation separately from execution.
    pub async fn run(self) -> Result<()> {
        match self.command {
            RootBenchCommand::Freeze { output, blocks, transactions_per_block, warmup } => {
                let result = (|| {
                    RootBenchBundle::numbered(&output)?;
                    RootBenchBundle::write_new(&output, &RootBenchBundle::freeze(blocks, transactions_per_block, warmup)?)
                })();
                result.map_err(|error| RootBenchExit::Refused.attach(error))
            }
            RootBenchCommand::Run(args) => args.run().await,
        }
    }
}

impl RootBenchBundle {
    /// Signs deterministic state-changing transfers outside the timed run.
    pub fn freeze(blocks: u64, transactions: u64, warmup: u64) -> Result<Self> {
        ensure!(blocks >= 4 && blocks <= 10_000 && transactions > 0 && transactions <= 1000 && warmup < blocks, "require 4..10000 blocks, 1..1000 transfers and warmup < blocks");
        // Well-known synthetic test-only key: 32 repeated 0x01 bytes. Never use for real funds.
        let signer: PrivateKeySigner = B256::repeat_byte(1).to_string().parse()?;
        let genesis = Self::transfer_genesis()?;
        let prepared = PreparedTransaction { native:false, from:signer.address(), to:Some(Address::repeat_byte(2)), value:U256::from(1), data:Bytes::new(), gas_limit:21_000, estimated_gas:21_000, validity:vec![], cohort:SubmitCohort::Plain };
        let mut attributes = Vec::new();
        for block in 0..blocks {
            let signed = (0..transactions).map(|tx| SubmissionPipeline::sign_at_nonce(&signer, &prepared, 900, block * transactions + tx, Fees { max_fee:2_000_000_000, priority_fee:1_000_000 }).map(|signed| signed.raw)).collect::<std::result::Result<Vec<_>, _>>()?;
            attributes.push(Self::attributes(block + 1, signed));
        }
        let mut bundle = Self { version:1, chain_id:900, genesis, activation:2, warmup, blocks:attributes, digest:B256::ZERO };
        bundle.digest = bundle.compute_digest()?;
        Ok(bundle)
    }

    /// Constructs the fixed isolated genesis without signing any transactions.
    pub fn transfer_genesis() -> Result<Genesis> {
        let sender = Self::freeze_identity()?;
        let mut genesis: Genesis = serde_json::from_value(json!({
            "config": {"chainId":900,"homesteadBlock":0,"eip150Block":0,"eip155Block":0,"eip158Block":0,"byzantiumBlock":0,"constantinopleBlock":0,"petersburgBlock":0,"istanbulBlock":0,"muirGlacierBlock":0,"berlinBlock":0,"londonBlock":0,"arrowGlacierBlock":0,"grayGlacierBlock":0,"mergeNetsplitBlock":0,"terminalTotalDifficulty":"0x0","terminalTotalDifficultyPassed":true,"bedrockBlock":0,"regolithTime":0,"canyonTime":0,"ecotoneTime":0,"fjordTime":0,"graniteTime":0,"holoceneTime":0,"isthmusTime":0,"jovianTime":0,"blockTime":1,"optimism":{"eip1559Elasticity":6,"eip1559Denominator":50}},
            "timestamp":"0x0","gasLimit":ROOT_BENCH_GAS_LIMIT,"difficulty":"0x0","baseFeePerGas":"0x3b9aca00","alloc":{sender.to_string():{"balance":"0xd3c21bcecceda1000000"}}
        }))?;
        let params = BaseChainSpec::from_genesis(genesis.clone()).base_fee_params_at_timestamp(0);
        genesis.extra_data = JovianExtraData::encode(B64::ZERO, params, 0)?;
        Ok(genesis)
    }

    /// Constructs exact fixed block attributes while retaining existing signed bytes unchanged.
    pub fn attributes(timestamp: u64, transactions: Vec<Bytes>) -> BasePayloadAttributes {
        BasePayloadAttributes {
            payload_attributes: PayloadAttributes { timestamp, prev_randao:B256::ZERO, suggested_fee_recipient:Address::ZERO, withdrawals:Some(vec![]), parent_beacon_block_root:Some(B256::ZERO), slot_number:None, target_gas_limit:None },
            transactions:Some(transactions), no_tx_pool:Some(true), gas_limit:Some(ROOT_BENCH_GAS_LIMIT),
            eip_1559_params:Some(B64::from((50u64 << 32) | 6)), min_base_fee:Some(1),
        }
    }

    /// Computes the integrity digest without a self-reference.
    pub fn compute_digest(&self) -> Result<B256> {
        let mut copy = self.clone(); copy.digest = B256::ZERO;
        Ok(keccak256(serde_json::to_vec(&copy)?))
    }

    /// Verifies frozen bytes, chain, contiguous nonces and exact supported transfer contract.
    pub fn validate(&self) -> Result<()> {
        ensure!(self.digest == self.compute_digest()?, "frozen bundle digest mismatch");
        ensure!(self.version == 1 && self.chain_id == 900 && self.genesis.config.chain_id == 900 && self.genesis.timestamp == 0 && self.activation == 2, "unsupported chain or activation");
        ensure!((4..=10_000).contains(&self.blocks.len()) && self.warmup < self.blocks.len() as u64, "invalid block/warmup range");
        ensure!(serde_json::to_vec(&self.genesis)? == serde_json::to_vec(&Self::transfer_genesis()?)?, "unsupported genesis allocation/forks");
        let expected = Self::freeze_identity()?;
        let mut nonce = 0;
        for (index, attrs) in self.blocks.iter().enumerate() {
            ensure!(attrs.payload_attributes.timestamp == index as u64 + 1 && attrs.no_tx_pool == Some(true) && attrs.gas_limit == Some(ROOT_BENCH_GAS_LIMIT), "invalid frozen attributes");
            let transactions = attrs.transactions.as_ref().ok_or_else(|| eyre!("missing forced transactions"))?;
            ensure!(!transactions.is_empty() && transactions.len() <= 1000, "empty or oversized block");
            ensure!(serde_json::to_vec(attrs)? == serde_json::to_vec(&Self::attributes(index as u64 + 1, transactions.clone()))?, "unsupported fixed attributes");
            for raw in transactions {
                let tx = TxEnvelope::decode_2718_exact(raw)?;
                ensure!(tx.chain_id() == Some(900) && tx.nonce() == nonce && tx.gas_limit() == 21_000 && tx.max_fee_per_gas() == 2_000_000_000 && tx.max_priority_fee_per_gas() == Some(1_000_000), "signed chain/nonce/gas/fee mismatch");
                ensure!(tx.recover_signer()? == expected && tx.to() == Some(Address::repeat_byte(2)) && tx.value() == U256::from(1) && tx.input().is_empty(), "unsupported signed workload");
                nonce += 1;
            }
        }
        ensure!(self.genesis.alloc.get(&expected).is_some_and(|account| account.balance >= U256::from(nonce) * U256::from(21_000u64 * 2_000_000_000 + 1)), "genesis must fund all signed nonces");
        ensure!(!self.genesis.config.extra_fields.contains_key("base"), "common genesis must not schedule D");
        Ok(())
    }

    /// Returns the deterministic disposable workload sender.
    /// Uses the well-known synthetic test-only key of 32 repeated 0x01 bytes, never real credentials.
    pub fn freeze_identity() -> Result<Address> {
        Ok(B256::repeat_byte(1).to_string().parse::<PrivateKeySigner>()?.address())
    }

    /// Refuses unnamed/non-numbered destinations rather than selecting or reusing old evidence.
    pub fn numbered(path: &Path) -> Result<()> {
        ensure!(path.file_stem().and_then(|name| name.to_str()).and_then(|name| name.chars().last()).is_some_and(|c| c.is_ascii_digit()), "destination must have a numbered basename");
        Ok(())
    }

    /// Writes a single immutable artifact with atomic create-new admission.
    pub fn write_new(path: &Path, value: &impl Serialize) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(value)?;
        let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
        file.write_all(&bytes)?; file.sync_all()?; Ok(())
    }
}

/// Parser for a closed, internally generated execution-node argument set.
#[derive(Debug, Parser)]
pub struct RootBenchNodeArgs {
    /// Reuse production CLI parsing rather than a second launch configuration model.
    #[command(flatten)]
    pub execution: ExecutionNodeArgs,
}

impl RootBenchCohort {
    /// Builds a CLI-derived admitted extension-free configuration; only D schedules activation.
    pub fn launch_config(self, bundle: &RootBenchBundle, datadir: &Path) -> Result<ExecutionNodeLaunchConfig> {
        let label = match self { Self::O => "ordinary", Self::S => "serial-trusted", Self::R => "serial-reexecution", Self::D => "deferred" };
        let parsed = RootBenchNodeArgs::try_parse_from(["root-bench", "--with-unused-ports", "--rollup.deferred-state-root-disposable", "--rollup.root-experiment-cohort", label, "--rollup.root-experiment-queue", "6", "--rollup.root-experiment-ready", "128", "--rollup.root-experiment-timeout-ms", "30000"])?;
        let mut genesis = bundle.genesis.clone();
        if self == Self::D { genesis.config.extra_fields.insert("base".into(), json!({"deferredStateRoot":bundle.activation})); }
        let mut launch = parsed.execution.into_launch_config(Arc::new(BaseChainSpec::from_genesis(genesis)));
        let rollup = &launch.standard.rpc.rollup_args;
        ensure!(rollup.sequencer.is_none() && !rollup.proofs_history && rollup.proofs_history_storage_path.is_none() && rollup.upgrade_signal.contract_address.is_none() && rollup.upgrade_signal_l1_rpc.upgrade_signal_l1_rpc.is_none() && launch.standard.rpc.rpc_forwarding_endpoint.is_none(), "all cohorts forbid production/L1/proof/forwarding environment configuration");
        launch = launch.with_unified_auth_endpoint();
        launch.node_config.datadir.datadir = datadir.to_path_buf().into();
        launch.node_config.debug.invalid_block_hook = None;
        launch.node_config.engine.persistence_threshold = 0;
        launch.node_config.engine.allow_unwind_canonical_header = true;
        launch.node_config.network.snap_v2 = false;
        launch.node_config.network.discovery.disable_discovery = true;
        launch.node_config.network.max_inbound_peers = Some(0);
        launch.node_config.network.max_outbound_peers = Some(0);
        ensure!(launch.node_config.prune_config().is_none(), "all benchmark cohorts require unpruned history");
        Ok(launch)
    }

    /// Counts controlled blocks, including warmup, rather than measured samples.
    pub fn active_blocks(self, bundle: &RootBenchBundle) -> u64 {
        bundle.blocks.iter().filter(|attrs| self == Self::R || (self == Self::D && attrs.payload_attributes.timestamp >= bundle.activation)).count() as u64
    }

    /// Requires actual independent first-import work; VALID/AlreadySeen is not evidence.
    pub fn check_accounting(self, before: &str, after: &str, active_blocks: u64) -> Result<()> {
        if !matches!(self, Self::R | Self::D) { return Ok(()); }
        for metric in ["builder_executions", "validator_executions", "current_roots_started{source=\"builder\"}", "current_roots_started{source=\"validator\"}", "validator_ready_outputs"] {
            let key = format!("reth_base_root_experiment_{metric}");
            ensure!(Self::counter(after, &key)? - Self::counter(before, &key)? == active_blocks as f64, "missing/excess aggregate independent first-import accounting: {key}");
        }
        let key = "reth_base_root_experiment_trusted_insertions";
        ensure!(Self::counter(after, key)? == 0.0 && Self::counter(before, key)? == 0.0, "R/D trusted insertion observed");
        Ok(())
    }

    /// Reads exact Prometheus counter identity, preserving absent zero counters.
    pub fn counter(raw: &str, key: &str) -> Result<f64> {
        for line in raw.lines() {
            if let Some((name, value)) = line.split_once(' ') { if name == key { return Ok(value.parse()?); } }
        }
        Ok(0.0)
    }
}

impl RootBenchSummary {
    /// Uses the replay LatencySummary nearest-rank convention with full tail ranks and counts.
    pub fn from_samples(samples: &[u64]) -> Self {
        if samples.is_empty() { return Self::default(); }
        let mut sorted = samples.to_vec(); sorted.sort_unstable();
        let rank = |p: usize| Some(sorted[(p * sorted.len()).div_ceil(100) - 1]);
        Self { count:sorted.len(), p50_ns:rank(50), p90_ns:rank(90), p95_ns:rank(95), p99_ns:rank(99) }
    }

    /// Returns actual measured Engine endpoints; cancellation never invents a canonical end.
    pub fn loop_bounds(samples: &[RootBenchSample]) -> (Option<u64>, Option<u64>) {
        let start = samples.iter().find(|s| !s.warmup).and_then(|s| s.phases.get("build_fcu")).map(|p| p.start_ns);
        let end = samples.iter().rev().find(|s| !s.warmup).filter(|s| s.pending.canonical_fcu.as_ref().is_some_and(|response| response.payload_status.status == PayloadStatusEnum::Valid && response.payload_status.latest_valid_hash == s.block_hash)).and_then(|s| s.phases.get("canonical_fcu")).map(|p| p.end_ns);
        (start, end)
    }

    /// Publishes verified totals over the saturated wall interval only for a complete workload.
    pub fn engine_saturated(samples: &[RootBenchSample], expected_measured: usize, complete: bool) -> Option<Value> {
        let measured: Vec<_> = samples.iter().filter(|s| !s.warmup).collect();
        if !complete || expected_measured == 0 || measured.len() != expected_measured || measured.iter().any(|s| s.error.is_some() || s.cycle_ns.is_none() || !s.pending.canonical_fcu.as_ref().is_some_and(|response| response.payload_status.status == PayloadStatusEnum::Valid && response.payload_status.latest_valid_hash == s.block_hash) || ["build_fcu", "get_payload", "new_payload", "canonical_fcu"].iter().any(|phase| !s.phases.contains_key(*phase))) { return None; }
        let (start, end) = Self::loop_bounds(samples);
        let window_ns = end?.checked_sub(start?)?;
        if window_ns == 0 { return None; }
        let gas_used: u64 = measured.iter().map(|s| s.accepted_gas).sum();
        let transactions: u64 = measured.iter().map(|s| s.successful_receipts).sum();
        Some(json!({"window_ns":window_ns,"measured_blocks":measured.len() as u64,"gas_used":gas_used,"transactions":transactions,"gas_per_second":gas_used as f64 * 1_000_000_000.0 / window_ns as f64,"transactions_per_second":transactions as f64 * 1_000_000_000.0 / window_ns as f64}))
    }

    /// Computes verified gas/s and receipts/s through drain using nanosecond wall time.
    pub fn throughput(samples: &[RootBenchSample], start_ns: Option<u64>, end_ns: u64) -> Option<(f64, f64)> {
        let duration = end_ns.checked_sub(start_ns?)?;
        if duration == 0 { return None; }
        let measured: Vec<_> = samples.iter().filter(|s| !s.warmup && s.error.is_none() && s.cycle_ns.is_some()).collect();
        if measured.is_empty() { return None; }
        let seconds = duration as f64 / 1_000_000_000.0;
        Some((measured.iter().map(|s| s.accepted_gas).sum::<u64>() as f64 / seconds, measured.iter().map(|s| s.successful_receipts).sum::<u64>() as f64 / seconds))
    }

    /// Excludes warmup and incomplete/error samples without inventing zero observations.
    pub fn phase(samples: &[RootBenchSample], phase: &str) -> Self {
        Self::from_samples(&samples.iter().filter(|s| !s.warmup && s.error.is_none()).filter_map(|s| if phase == "cycle" { s.cycle_ns } else { s.phases.get(phase).map(|i| i.end_ns - i.start_ns) }).collect::<Vec<_>>())
    }
}

impl RootBenchSample {
    /// Creates a pending sample without success values.
    pub fn new(number: u64, warmup: bool) -> Self {
        Self { number, warmup, parent_hash:B256::ZERO, block_hash:None, phases:BTreeMap::new(), cycle_ns:None, responses:BTreeMap::new(), payload_reference:None, pending:RootBenchPending::default(), error:None, accepted_gas:0, successful_receipts:0 }
    }

    /// Sums raw phase durations without inventing a monotonic endpoint.
    pub fn finish_cycle(&mut self) {
        self.cycle_ns = if self.error.is_none() { ["build_fcu", "get_payload", "new_payload", "canonical_fcu"].into_iter().try_fold(0u64, |total, phase| self.phases.get(phase).and_then(|interval| total.checked_add(interval.end_ns.checked_sub(interval.start_ns)?))) } else { None };
    }

    /// Drives only bounded Engine calls and retains typed evidence. Verification follows saturation.
    pub async fn drive<E: RootBenchEngine>(engine: &E, attributes: Vec<BasePayloadAttributes>, context: RootBenchDrive, samples: &mut Vec<Self>, measurement_start: &mut Option<u64>) -> Result<()> {
        let genesis = context.genesis;
        let warmup = context.warmup;
        let origin = context.origin;
        let timeout = context.timeout;
        samples.reserve(attributes.len());
        let mut parent = genesis;
        for (index, attrs) in attributes.into_iter().enumerate() {
            if index as u64 == warmup { *measurement_start = Some(origin.elapsed().as_nanos() as u64); }
            samples.push(Self::new(index as u64 + 1, (index as u64) < warmup));
            let sample = samples.last_mut().ok_or_else(|| RootBenchExit::Incomplete.attach(eyre!("retained sample missing")))?;
            sample.parent_hash = parent;
            let result: Result<B256> = async {
                let fcu = ForkchoiceState { head_block_hash:parent, safe_block_hash:parent, finalized_block_hash:genesis };
                sample.pending.build_fcu = Some(sample.call("build_fcu", origin, timeout, engine.fcu(fcu, Some(attrs))).await?);
                let started = sample.pending.build_fcu.as_ref().ok_or_else(|| RootBenchExit::Incomplete.attach(eyre!("build response missing")))?;
                Self::require_valid(&started.payload_status, parent)?;
                let id = started.payload_id.ok_or_else(|| eyre!("build FCU missing payload ID"))?;
                // AwaitInProgress resolves construction. There is deliberately no scheduling sleep.
                sample.pending.envelope = Some(sample.call("get_payload", origin, timeout, engine.payload(id)).await?);
                let envelope = sample.pending.envelope.as_ref().ok_or_else(|| RootBenchExit::Incomplete.attach(eyre!("payload missing")))?;
                let hash = envelope.execution_payload.payload_inner.payload_inner.payload_inner.block_hash;
                sample.block_hash = Some(hash);
                // Import consumes a clone, prepared before the import phase clock.
                let payload = envelope.execution_payload.clone();
                let beacon = envelope.parent_beacon_block_root;
                sample.pending.new_payload = Some(sample.call("new_payload", origin, timeout, engine.import(payload, beacon)).await?);
                Self::require_valid(sample.pending.new_payload.as_ref().ok_or_else(|| RootBenchExit::Incomplete.attach(eyre!("import response missing")))?, hash)?;
                sample.pending.canonical_fcu = Some(sample.call("canonical_fcu", origin, timeout, engine.fcu(ForkchoiceState { head_block_hash:hash, ..fcu }, None)).await?);
                Self::require_valid(&sample.pending.canonical_fcu.as_ref().ok_or_else(|| RootBenchExit::Incomplete.attach(eyre!("canonical response missing")))?.payload_status, hash)?;
                Ok(hash)
            }.await;
            match result { Ok(hash) => parent = hash, Err(error) => { sample.error = Some(format!("{error:#}")); return Err(error); } }
        }
        Ok(())
    }

    /// Checks native identity/order and D's activation commitment against the actual pre-F root.
    pub fn verify_block(block: &alloy_consensus::Block<Bytes>, attrs: &BasePayloadAttributes, parent: B256, number: u64, cohort: RootBenchCohort, activation: u64, activation_parent_root: Option<B256>) -> Result<()> {
        ensure!(block.header.parent_hash == parent && block.header.number == number && block.header.timestamp == attrs.payload_attributes.timestamp, "native payload identity mismatch");
        ensure!(Some(&block.body.transactions) == attrs.transactions.as_ref(), "native payload changed signed transaction bytes/order");
        if cohort == RootBenchCohort::D && attrs.payload_attributes.timestamp == activation {
            ensure!(activation_parent_root == Some(block.header.state_root), "D activation must commit the actual pre-F parent post-state");
        }
        Ok(())
    }

    /// Reconstructs all V4 header fields and checks the advertised native block identity.
    pub fn payload_block(payload: BaseExecutionPayloadV4, beacon_root: B256) -> Result<alloy_consensus::Block<Bytes>> {
        let data = ExecutionData::v4(payload, vec![], beacon_root, Default::default());
        let advertised = data.payload.block_hash();
        let block = data.payload.into_block_with_sidecar_raw(&data.sidecar)?;
        ensure!(block.header.hash_slow() == advertised, "native payload advertised hash mismatch");
        Ok(block)
    }

    /// Verifies native receipts in canonical block transaction order without L1 RPC enrichment.
    pub fn verify_receipts(receipts: &[base_common_consensus::BaseReceipt], transactions: usize, block_gas: u64) -> Result<()> {
        ensure!(receipts.len() == transactions, "native receipt count differs from accepted transactions");
        let mut previous = 0;
        for receipt in receipts {
            ensure!(receipt.status() && receipt.cumulative_gas_used().checked_sub(previous) == Some(21_000), "native receipt status/gas/order mismatch");
            previous = receipt.cumulative_gas_used();
        }
        ensure!(previous == block_gas, "native receipt cumulative gas differs from accepted block");
        Ok(())
    }

    /// Serializes retained typed responses only after the sequence completes or fails.
    pub fn flush_pending(&mut self) -> Result<()> {
        if let Some(response) = &self.pending.build_fcu { self.responses.insert("build_fcu".into(), serde_json::to_value(response)?); }
        if let Some(response) = &self.pending.new_payload { self.responses.insert("new_payload".into(), serde_json::to_value(response)?); }
        if let Some(response) = &self.pending.canonical_fcu { self.responses.insert("canonical_fcu".into(), serde_json::to_value(response)?); }
        if self.pending.envelope.is_some() {
            let reference = json!({"file":"samples.jsonl","number":self.number,"selection":"last-row-per-number","pointer":"/responses/payload"});
            self.responses.insert("payload".into(), reference.clone());
            self.payload_reference = Some(reference);
        }
        if let Some(header) = &self.pending.payload_header { self.responses.insert("payload_header".into(), serde_json::to_value(header)?); }
        Ok(())
    }

    /// Attempts every buffered sample independently; evidence failures never short-circuit drain.
    pub fn flush_all(samples: &mut [Self], output: &Path, outcome: &mut Result<()>) {
        Self::flush_all_with(samples, outcome, |sample| sample.persist(output));
    }

    /// Shares independent row attempts with fault-injection tests of serialization failures.
    pub fn flush_all_with(samples: &mut [Self], outcome: &mut Result<()>, mut persist: impl FnMut(&Self) -> Result<()>) {
        for sample in samples {
            sample.finish_cycle();
            if let (Some(build), Some(payload)) = (sample.phases.get("build_fcu"), sample.phases.get("get_payload")) {
                sample.phases.insert("payload_available".into(), RootBenchInterval { start_ns:build.start_ns, end_ns:payload.end_ns });
            }
            RootBenchCleanup::retain_error(outcome, std::panic::catch_unwind(AssertUnwindSafe(|| sample.flush_pending())).unwrap_or_else(|_| Err(RootBenchExit::Incomplete.attach(eyre!("sample response serialization panicked")))));
            RootBenchCleanup::retain_error(outcome, std::panic::catch_unwind(AssertUnwindSafe(|| persist(sample))).unwrap_or_else(|_| Err(RootBenchExit::Incomplete.attach(eyre!("sample persistence panicked")))));
        }
    }

    /// Requires the exact accepted identity as well as VALID status.
    pub fn require_valid(status: &PayloadStatus, hash: B256) -> Result<()> {
        ensure!(status.status == PayloadStatusEnum::Valid && status.latest_valid_hash == Some(hash), "Engine acceptance mismatch: expected {hash}, received {status:?}"); Ok(())
    }

    /// Appends a flushed phase snapshot without overwriting earlier evidence.
    pub fn persist(&self, output: &Path) -> Result<()> {
        let mut file = OpenOptions::new().append(true).create(true).open(output.join("samples.jsonl"))?;
        let result = std::panic::catch_unwind(AssertUnwindSafe(|| serde_json::to_writer(&mut file, &RootBenchSampleRecord { sample:self }))).map_err(|_| RootBenchExit::Incomplete.attach(eyre!("sample serialization panicked"))).and_then(|result| result.map_err(Into::into));
        // Terminate even a failed row so independent later samples remain recoverable JSONL.
        let newline = file.write_all(b"\n");
        let durable = file.sync_data();
        result?; newline?; durable?; Ok(())
    }

    /// Times one bounded call without serialization or disk I/O between Engine phases.
    pub async fn call<T>(&mut self, name: &str, origin: Instant, timeout: Duration, future: impl Future<Output = Result<T>>) -> Result<T> {
        let start_ns = origin.elapsed().as_nanos() as u64;
        let result = tokio::time::timeout(timeout, future).await.map_err(|_| RootBenchExit::Timeout.attach(eyre!("{name} timed out"))).and_then(|result| result);
        self.phases.insert(name.into(), RootBenchInterval { start_ns, end_ns:origin.elapsed().as_nanos() as u64 });
        if let Err(error) = &result { self.error = Some(format!("{error:#}")); }
        result
    }
}

impl RootBenchRunArgs {
    /// Converts unwinding active work to a typed failure so retained state can be drained.
    pub async fn catch_active<T>(future: impl Future<Output = Result<T>>) -> Result<T> {
        AssertUnwindSafe(future).catch_unwind().await.unwrap_or_else(|_| Err(RootBenchExit::Incomplete.attach(eyre!("active trial panicked; in-flight outcome uncertain"))))
    }

    /// Hashes a byte stream using a fixed 64-KiB buffer, without whole-file allocation.
    pub fn hash_reader(reader: &mut impl Read) -> Result<B256> {
        let mut hash = alloy_primitives::Keccak256::new();
        let mut buffer = [0u8; 65536];
        loop { let count = reader.read(&mut buffer)?; if count == 0 { break; } hash.update(&buffer[..count]); }
        Ok(hash.finalize())
    }

    /// Caches success and failure exactly once, including under parallel preflight callers.
    pub fn cached_digest(cache: &OnceLock<std::result::Result<B256, String>>, read: impl FnOnce() -> Result<B256>) -> Result<B256> {
        cache.get_or_init(|| read().map_err(|error| format!("{error:#}"))).as_ref().copied().map_err(|error| eyre!("executable digest unavailable: {error}"))
    }

    /// Opens the running executable once per process; all artifacts reuse the cached result.
    pub fn executable_digest() -> Result<B256> {
        static DIGEST: OnceLock<std::result::Result<B256, String>> = OnceLock::new();
        Self::cached_digest(&DIGEST, || Self::hash_reader(&mut fs::File::open(std::env::current_exe()?)?))
    }

    /// Fingerprints every captured environment value, including apparently benign settings.
    pub fn environment_fingerprints(values: impl IntoIterator<Item = (String, String)>) -> BTreeMap<String, String> {
        values.into_iter().filter(|(name, _)| name.starts_with("RETH_") || name.starts_with("BASE_")).map(|(name, value)| (name, format!("keccak:{}", keccak256(value)))).collect()
    }

    /// Fingerprints whole argument tokens, including values embedded in flags or paths.
    pub fn argument_fingerprints(values: impl IntoIterator<Item = std::ffi::OsString>) -> Vec<String> {
        values.into_iter().map(|arg| format!("keccak:{}", keccak256(arg.as_encoded_bytes()))).collect()
    }

    /// Records only explicit nonsecret scalar settings, never CLI-derived configuration Debug.
    pub fn launch_settings(launch: &ExecutionNodeLaunchConfig) -> Value {
        json!({"unpruned":launch.node_config.prune_config().is_none(),"queue":launch.standard.rpc.rollup_args.root_experiment_queue,"retention":launch.standard.rpc.rollup_args.root_experiment_ready,"persistence_threshold":launch.node_config.engine.persistence_threshold,"allow_unwind_canonical_header":launch.node_config.engine.allow_unwind_canonical_header,"snap_v2":launch.node_config.network.snap_v2,"disable_discovery":launch.node_config.network.discovery.disable_discovery,"max_inbound_peers":launch.node_config.network.max_inbound_peers,"max_outbound_peers":launch.node_config.network.max_outbound_peers,"witness_hook_configured":launch.node_config.debug.invalid_block_hook.is_some()})
    }

    /// Refuses unverified or mismatched external executable-to-source provenance.
    pub fn verify_manifest(manifest: Option<&Value>, executable: B256) -> Result<()> {
        let manifest = manifest.ok_or_else(|| eyre!("external executable provenance unverified; supply --executable-manifest"))?;
        ensure!(manifest.get("executable_keccak").and_then(Value::as_str).and_then(|s| s.parse::<B256>().ok()) == Some(executable), "executable manifest digest mismatch");
        ensure!(manifest.get("clean_source").and_then(Value::as_bool) == Some(true), "executable manifest requires clean source attestation");
        for field in ["source_commit", "source_tree"] {
            ensure!(manifest.get(field).and_then(Value::as_str).is_some_and(|s| s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit())), "missing/invalid manifest {field}");
        }
        for field in ["cargo_lock_keccak", "reth_pin_keccak"] {
            ensure!(manifest.get(field).and_then(Value::as_str).and_then(|s| s.parse::<B256>().ok()).is_some(), "missing/invalid manifest {field}");
        }
        Ok(())
    }

    /// Requires actual worker join and healthy manager state after graceful guards finish.
    pub fn verify_worker(manager: Option<&Arc<PostStateRootManager>>, expected: bool) -> Result<()> {
        if let Some(manager) = manager {
            manager.check_health()?;
            ensure!(manager.worker.try_lock().map_err(|e| eyre!("root worker join still in progress: {e}"))?.is_none(), "root worker has not been joined");
            ensure!(manager.queue.0.lock().map_err(PostStateRootManager::error)?.closed, "root queue admission remains open");
        } else { ensure!(!expected, "active root cohort has no observed manager"); }
        Ok(())
    }

    /// Reserves a fresh output directory and records even pre-launch failures without reuse.
    pub async fn run(self) -> Result<()> {
        RootBenchBundle::numbered(&self.output).map_err(|error| RootBenchExit::Refused.attach(error))?;
        fs::create_dir(&self.output).map_err(|error| RootBenchExit::Refused.attach(error.into()))?;
        let mut outcome = self.execute().await.map_err(|error| RootBenchExit::Refused.attach(error));
        if let Err(error) = &outcome {
            if !self.output.join("result.json").exists() {
                let fallback = RootBenchBundle::write_new(&self.output.join("result.json"), &json!({"schema_version":1,"complete":false,"drained":false,"error":format!("{error:#}"),"cohort":self.cohort}));
                RootBenchCleanup::retain_error(&mut outcome, fallback);
            }
        }
        outcome
    }

    /// Runs one bounded trial and always attempts explicit runtime drain, retaining failures.
    pub async fn execute(&self) -> Result<()> {
        static ACTIVE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
        ensure!(self.call_timeout_ms > 0 && self.run_timeout_ms > 0 && self.shutdown_timeout_ms > 0, "deadlines must be positive");
        RootBenchBundle::numbered(&self.output)?;
        let input_bytes = fs::read(&self.input)?;
        let bundle: RootBenchBundle = serde_json::from_slice(&input_bytes)?;
        bundle.validate()?;
        // Allocation is intentionally outside the writable node directory and is never reset.
        RootBenchBundle::write_raw_new(&self.output.join("input.json"), &input_bytes)?;
        let datadir = self.output.join("datadir1");
        fs::create_dir(&datadir)?;
        let launch = self.cohort.launch_config(&bundle, &datadir)?;
        let source = std::process::Command::new("git").args(["rev-parse", "HEAD", "HEAD^{tree}"]).output()?;
        ensure!(source.status.success(), "source provenance requires a git checkout");
        let checkout = std::process::Command::new("git").args(["rev-parse", "--show-toplevel"]).output()?;
        ensure!(checkout.status.success(), "runtime diagnostics require a git checkout");
        let checkout = PathBuf::from(String::from_utf8(checkout.stdout)?.trim());
        let executable_digest = Self::executable_digest()?;
        let manifest_bytes = fs::read(&self.executable_manifest).map_err(|error| eyre!("external executable provenance unverified; manifest read failed: {error}"))?;
        let manifest: Value = serde_json::from_slice(&manifest_bytes)?;
        RootBenchBundle::write_new(&self.output.join("executable-source.json"), &json!({"schema_version":1,"external_manifest":manifest,"verification":"external-attestation-required","executable_keccak":executable_digest}))?;
        Self::verify_manifest(Some(&manifest), executable_digest)?;
        // Independent preflight failures must not contend for the process-wide trial owner.
        let _active = ACTIVE.try_lock().map_err(|_| eyre!("only one root trial per process at a time"))?;
        RootBenchBundle::write_new(&self.output.join("executable.json"), &json!({"schema_version":1,"executable_keccak":executable_digest,"harness_module_keccak":keccak256(include_bytes!("root_bench.rs"))}))?;
        let environment = Self::environment_fingerprints(std::env::vars());
        RootBenchBundle::write_new(&self.output.join("launch.json"), &json!({"schema_version":1,"cohort":self.cohort,"transport":"typed-in-memory","receipt_source":"canonical-execution-provider-receipts_by_block","account_source":"canonical-execution-provider-state_by_block_hash/basic_account","config":Self::launch_settings(&launch),"runtime_checkout":{"source":String::from_utf8(source.stdout)?,"cargo_lock_keccak":keccak256(fs::read(checkout.join("Cargo.lock"))?),"reth_pin_keccak":keccak256(fs::read(checkout.join("etc/upstream-pins/reth.toml"))?),"not_executable_provenance":true},"argv":Self::argument_fingerprints(std::env::args_os()),"environment":environment,"unsupported":["cpu_time","rss","io","cache_hit_miss","persistence_lag","queue_depth","paced_cadence","immutable_6268_baseline"]}))?;
        // Share Reth's idempotent recorder installation with the admitted node launcher.
        let metrics = reth_node_metrics::recorder::install_prometheus_recorder().handle().clone();
        let origin = Instant::now();
        RootBenchBundle::write_new(&self.output.join("runtime.json"), &json!({"schema_version":1,"configuration":format!("{:#?}", RuntimeConfig::default())}))?;
        let runtime = std::thread::spawn(|| RuntimeBuilder::new(RuntimeConfig::default()).build()).join().map_err(|_| eyre!("runtime construction panicked"))??;
        let Some(mut critical) = runtime.take_task_manager_handle() else {
            let mut outcome = Err(eyre!("runtime critical-task monitor unavailable"));
            let drain = RootBenchCleanup { runtime, critical:None, critical_result:None, manager:None, manager_required:self.cohort != RootBenchCohort::O, receipt_tasks:Vec::new(), timeout:Duration::from_millis(self.shutdown_timeout_ms) }.finish(None, &self.output, &mut outcome).await;
            RootBenchCleanup::retain_error(&mut outcome, RootBenchBundle::write_new(&self.output.join("drain.json"), &drain));
            return outcome.map_err(|error| RootBenchExit::Incomplete.attach(error));
        };
        let mut launched = None;
        let mut samples = Vec::with_capacity(bundle.blocks.len());
        let attributes = bundle.blocks.clone();
        let mut total_transactions = 0;
        let cumulative_transactions: Vec<usize> = bundle.blocks.iter().map(|attrs| { total_transactions += attrs.transactions.as_ref().map_or(0, Vec::len); total_transactions }).collect();
        let mut before_metrics = None;
        let mut measurement_start_ns = None;
        let timeout = Duration::from_millis(self.call_timeout_ms);
        // Keep blocking read ownership across cancellation; only the owned runtime may run it.
        let mut receipt_tasks = Vec::new();
        let mut critical_result = None;
        let mut outcome = Self::catch_active(async { tokio::select! {
            result = tokio::time::timeout(Duration::from_millis(self.run_timeout_ms), async {
            launched = Some(tokio::time::timeout(timeout, launch.launch_default(CliContext { task_executor: runtime.clone() })).await.map_err(|_| RootBenchExit::Timeout.attach(eyre!("launch timed out")))??);
            let node = launched.as_ref().ok_or_else(|| eyre!("launch handle missing"))?;
            let read_module = node.auth_module.clone();
            let reads = RootProvider::<Base>::new(RpcClient::new(InMemoryTransport { module:read_module }, false));
            let initial = tokio::time::timeout(timeout, reads.get_block_by_number(alloy_eips::BlockNumberOrTag::Latest)).await.map_err(|_| RootBenchExit::Timeout.attach(eyre!("genesis read timed out")))??.ok_or_else(|| eyre!("missing genesis"))?;
            ensure!(initial.header.number == 0, "fresh-genesis benchmark cannot resume a database");
            let before = Self::root_metrics(&metrics);
            before_metrics = Some(before.clone());
            RootBenchBundle::write_raw_new(&self.output.join("metrics-before.txt"), before.as_bytes())?;
            RootBenchSample::drive(&node.engine_api, attributes, RootBenchDrive { genesis:initial.header.hash, warmup:bundle.warmup, origin, timeout }, &mut samples, &mut measurement_start_ns).await?;
            let mut evidence = Ok(());
            RootBenchSample::flush_all(&mut samples, &self.output, &mut evidence);
            evidence?;
            // No provider work, reconstruction, scrapes, serialization or filesystem work occurs
            // between canonical acceptance and the next build. All checks below follow saturation.
            let mut activation_parent_root = Some(initial.header.state_root);
            for (index, sample) in samples.iter_mut().enumerate() {
                sample.finish_cycle();
                let attrs = &bundle.blocks[index];
                let verification: Result<()> = async {
                let envelope = sample.pending.envelope.as_ref().ok_or_else(|| eyre!("missing retained payload"))?;
                let block = RootBenchSample::payload_block(envelope.execution_payload.clone(), envelope.parent_beacon_block_root)?;
                let hash = sample.block_hash.ok_or_else(|| eyre!("missing accepted hash"))?;
                RootBenchSample::verify_block(&block, attrs, sample.parent_hash, sample.number, self.cohort, bundle.activation, activation_parent_root)?;
                let canonical = tokio::time::timeout(timeout, reads.get_block_by_number(sample.number.into())).await.map_err(|_| RootBenchExit::Timeout.attach(eyre!("canonical read timed out")))??.ok_or_else(|| eyre!("missing accepted canonical block"))?;
                ensure!(canonical.header.hash == hash, "canonical block differs from accepted hash");
                if attrs.payload_attributes.timestamp < bundle.activation { activation_parent_root = Some(block.header.state_root); }
                sample.pending.payload_header = Some(block.header.clone());
                let provider = node.handle.node.provider.clone();
                let sender = RootBenchBundle::freeze_identity()?;
                let expected_nonce = cumulative_transactions[index];
                let transaction_count = attrs.transactions.as_ref().ok_or_else(|| eyre!("missing transactions"))?.len();
                let gas_used = block.header.gas_used;
                receipt_tasks.push(runtime.spawn_blocking(move || -> Result<u64> {
                    let receipts = provider.receipts_by_block(hash.into())?.ok_or_else(|| eyre!("missing canonical native receipts for {hash}"))?;
                    RootBenchSample::verify_receipts(&receipts, transaction_count, gas_used)?;
                    let state = provider.state_by_block_hash(hash)?;
                    let sender_nonce = AccountReader::basic_account(&state, &sender)?.unwrap_or_default().nonce;
                    ensure!(sender_nonce == expected_nonce as u64, "accepted sender nonce mismatch");
                    let recipient_balance = AccountReader::basic_account(&state, &Address::repeat_byte(2))?.unwrap_or_default().balance;
                    ensure!(recipient_balance == U256::from(expected_nonce), "recipient balance must reflect every accepted signed transfer exactly");
                    Ok(receipts.len() as u64)
                }));
                let task = receipt_tasks.last_mut().ok_or_else(|| RootBenchExit::Incomplete.attach(eyre!("retained receipt/account task missing")))?;
                let checked = tokio::time::timeout(timeout, task).await.map_err(|_| RootBenchExit::Timeout.attach(eyre!("provider verification timed out")))?;
                receipt_tasks.pop();
                sample.successful_receipts = checked??;
                sample.accepted_gas = gas_used;
                Ok(())
                }.await;
                if let Err(error) = verification { sample.error = Some(format!("{error:#}")); return Err(error); }
            }
            let head = tokio::time::timeout(timeout, reads.get_block_by_number(alloy_eips::BlockNumberOrTag::Latest)).await.map_err(|_| RootBenchExit::Timeout.attach(eyre!("final head read timed out")))??.ok_or_else(|| eyre!("missing final head"))?;
            ensure!(samples.last().and_then(|s| s.block_hash) == Some(head.header.hash), "final head differs from final accepted hash");
            Ok::<(), eyre::Report>(())
            }) => result.map_err(|_| RootBenchExit::Timeout.attach(eyre!("overall trial deadline exceeded; in-flight call outcome uncertain"))).and_then(|result| result),
            failure = &mut critical => { let error = eyre!("runtime critical-task monitor ended during trial: {failure:?}"); critical_result = Some(failure); Err(error) },
            _ = tokio::signal::ctrl_c() => Err(eyre!("trial interrupted; in-flight call outcome uncertain")),
        } }).await;
        if let Err(error) = &outcome {
            if let Some(last) = samples.last_mut() { last.error.get_or_insert_with(|| format!("{error:#}")); }
        }
        RootBenchSample::flush_all(&mut samples, &self.output, &mut outcome);
        // Releasing transports/providers before graceful shutdown is part of the measurement.
        let service = launched.as_ref().and_then(|node| node.root_service.clone());
        drop(launched);
        let manager = service.as_ref().and_then(|service| service.manager.get()).cloned();
        let drain = RootBenchCleanup { runtime, critical:Some(critical), critical_result, manager, manager_required:self.cohort != RootBenchCohort::O, receipt_tasks, timeout:Duration::from_millis(self.shutdown_timeout_ms) }.finish(None, &self.output, &mut outcome).await;
        let drained = drain["drained"] == true;
        let end_ns = origin.elapsed().as_nanos() as u64;
        let drained_metrics = Self::root_metrics(&metrics);
        RootBenchCleanup::retain_error(&mut outcome, RootBenchBundle::write_raw_new(&self.output.join("metrics-drained.txt"), drained_metrics.as_bytes()));
        let metric_result = RootBenchCohort::counter(&drained_metrics, "reth_base_root_experiment_jobs_failed").and_then(|failures| { ensure!(failures == 0.0, "root worker failure observed after drain"); Ok(()) });
        RootBenchCleanup::retain_error(&mut outcome, metric_result);
        if let Some(before) = before_metrics { RootBenchCleanup::retain_error(&mut outcome, self.cohort.check_accounting(&before, &drained_metrics, self.cohort.active_blocks(&bundle))); }
        RootBenchSample::flush_all(&mut samples, &self.output, &mut outcome);
        let phases = ["build_fcu", "get_payload", "payload_available", "new_payload", "canonical_fcu", "cycle"];
        let summaries: BTreeMap<_, _> = phases.into_iter().map(|phase| (phase, RootBenchSummary::phase(&samples, phase))).collect();
        let throughput = RootBenchSummary::throughput(&samples, measurement_start_ns, end_ns);
        RootBenchCleanup::retain_error(&mut outcome, RootBenchBundle::write_new(&self.output.join("drain.json"), &drain));
        let (loop_start_ns, loop_end_ns) = RootBenchSummary::loop_bounds(&samples);
        let engine_saturated = RootBenchSummary::engine_saturated(&samples, bundle.blocks.len() - bundle.warmup as usize, outcome.is_ok() && drained);
        let result = RootBenchBundle::write_new(&self.output.join("result.json"), &json!({"schema_version":1,"complete":outcome.is_ok() && drained,"error":outcome.as_ref().err().map(|e| format!("{e:#}")),"drained":drained,"cohort":self.cohort,"input_digest":bundle.digest,"samples":samples,"summaries":summaries,"measurement_start_ns":measurement_start_ns,"drain_end_ns":end_ns,"loop_start_ns":loop_start_ns,"loop_end_ns":loop_end_ns,"engine_saturated":engine_saturated,"accepted_gas_per_second":throughput.map(|value| value.0),"successful_receipts_per_second":throughput.map(|value| value.1)}));
        RootBenchCleanup::retain_error(&mut outcome, result);
        outcome.map_err(|error| RootBenchExit::Incomplete.attach(error))?; ensure!(drained, "trial did not drain"); Ok(())
    }

    /// Preserves process-level experiment series verbatim in the exporter's representation.
    pub fn root_metrics(handle: &PrometheusHandle) -> String {
        handle.render().lines().filter(|line| line.starts_with("reth_base_root_experiment_") || line.contains(" reth_base_root_experiment_")).map(|line| format!("{line}\n")).collect()
    }
}

impl RootBenchBundle {
    /// Creates a raw evidence file without overwriting a previous sample.
    pub fn write_raw_new(path: &Path, bytes: &[u8]) -> Result<()> {
        let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
        file.write_all(bytes)?; file.sync_all()?; Ok(())
    }
}

#[cfg(test)]
mod tests {
    use base_execution_payload_builder::{PostStateRootManager, PostStateRootLimits};
    use super::*;
    use clap::Parser;

    /// Saturated rates use raw measured Engine boundaries, not drain or phase sums.
    #[test]
    fn saturated_wall_interval_and_fail_closed_rates() {
        let mut warmup = RootBenchSample::new(1, true);
        warmup.phases.insert("build_fcu".into(), RootBenchInterval { start_ns:1, end_ns:2 });
        let mut measured = RootBenchSample::new(2, false);
        for (phase, start, end) in [("build_fcu", 10, 20), ("get_payload", 30, 40), ("new_payload", 50, 60), ("canonical_fcu", 70, 2_000_000_010)] {
            measured.phases.insert(phase.into(), RootBenchInterval { start_ns:start, end_ns:end });
        }
        measured.finish_cycle(); measured.accepted_gas = 42_000; measured.successful_receipts = 2;
        measured.block_hash = Some(B256::repeat_byte(2));
        measured.pending.canonical_fcu = Some(alloy_rpc_types_engine::ForkchoiceUpdated { payload_status:PayloadStatus::new(PayloadStatusEnum::Valid, measured.block_hash), payload_id:None });
        let mut samples = vec![warmup, measured];
        assert_eq!(RootBenchSummary::loop_bounds(&samples), (Some(10), Some(2_000_000_010)), "wall bounds must equal actual measured Engine endpoints");
        let expected = json!({"window_ns":2_000_000_000u64,"measured_blocks":1,"gas_used":42_000,"transactions":2,"gas_per_second":21_000.0,"transactions_per_second":1.0});
        assert_eq!(RootBenchSummary::engine_saturated(&samples, 1, true), Some(expected), "saturation must include gaps but exclude all post-loop work");
        assert_eq!(RootBenchSummary::engine_saturated(&samples, 2, true), None, "partial successful prefix must not pass the expected measured count");
        assert_eq!(RootBenchSummary::engine_saturated(&samples, 1, false), None, "incomplete trial must never publish saturated rates");
        samples[1].error = Some("verification failed".into());
        assert_eq!(RootBenchSummary::engine_saturated(&samples, 1, true), None, "verification failure must invalidate rates");
        samples[1].error = None; samples[1].phases.remove("canonical_fcu");
        assert_eq!(RootBenchSummary::engine_saturated(&samples, 1, true), None, "cancelled in-flight canonical acceptance must not count");
        samples[1].pending.canonical_fcu = None;
        assert_eq!(RootBenchSummary::loop_bounds(&samples), (Some(10), None), "unaccepted canonical response must not invent a successful end");
        assert_eq!(RootBenchSummary::loop_bounds(&[]), (None, None), "absent loop has no invented endpoints");
        samples[1].pending.canonical_fcu = Some(alloy_rpc_types_engine::ForkchoiceUpdated { payload_status:PayloadStatus::new(PayloadStatusEnum::Valid, samples[1].block_hash), payload_id:None });
        samples[1].phases.insert("canonical_fcu".into(), RootBenchInterval { start_ns:70, end_ns:2_000_000_010 });
        samples.push(RootBenchSample::new(3, false));
        assert_eq!(RootBenchSummary::loop_bounds(&samples).1, None, "an unfinished last measured block must not fall back to an earlier canonical endpoint");
    }

    /// Every retained sample is attempted on failure, including independent filesystem errors.
    #[test]
    fn buffered_flush_attempts_every_sample() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let mut samples = vec![RootBenchSample::new(1, false), RootBenchSample::new(2, false), RootBenchSample::new(3, false)];
        // Valid earlier responses must survive alongside later incomplete samples.
        samples[0].pending.build_fcu = Some(alloy_rpc_types_engine::ForkchoiceUpdated { payload_status:PayloadStatus::new(PayloadStatusEnum::Valid, Some(B256::ZERO)), payload_id:None });
        let mut outcome = Err(eyre::Report::new(RootBenchExit::Timeout));
        RootBenchSample::flush_all(&mut samples, directory.path(), &mut outcome);
        let rows: Vec<Value> = fs::read_to_string(directory.path().join("samples.jsonl"))?.lines().map(serde_json::from_str).collect::<std::result::Result<_, _>>()?;
        assert_eq!(rows.iter().map(|row| row["number"].as_u64().unwrap()).collect::<Vec<_>>(), vec![1, 2, 3], "failure cleanup must persist every sample in order");
        assert_eq!(RootBenchExit::code(&outcome), 4, "evidence writes must preserve the observed deadline category");
        let mut failed = Ok(());
        RootBenchSample::flush_all(&mut samples, &directory.path().join("missing"), &mut failed);
        assert_eq!(format!("{:#}", failed.unwrap_err()).matches("subsequent failure").count(), 2, "three independent failed writes must all be attempted");
        let injected = tempfile::tempdir()?;
        let mut attempts = Vec::new();
        RootBenchSample::flush_all_with(&mut samples, &mut outcome, |sample| {
            attempts.push(sample.number);
            if sample.number == 1 { return RootBenchBundle::write_new(&injected.path().join("invalid1.json"), &BTreeMap::from([((1u8, 2u8), 3u8)])); }
            sample.persist(injected.path())
        });
        let rows: Vec<Value> = fs::read_to_string(injected.path().join("samples.jsonl"))?.lines().map(serde_json::from_str).collect::<std::result::Result<_, _>>()?;
        assert_eq!(attempts, vec![1, 2, 3], "a real serde failure must not skip later row attempts");
        assert_eq!(rows.iter().map(|row| row["number"].as_u64().unwrap()).collect::<Vec<_>>(), vec![2, 3], "later rows must persist after serialization failure");
        assert_eq!(RootBenchExit::code(&outcome), 4, "serialization failure must not erase observed timeout category");
        Ok(())
    }

    /// Full native payloads remain in JSONL while result summaries reference them.
    #[test]
    fn payload_reference_resolves_without_result_duplication() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let mut sample = RootBenchSample::new(7, false);
        sample.pending.envelope = Some(SelfFixture::envelope(B256::repeat_byte(7)));
        sample.flush_pending()?; sample.persist(directory.path())?;
        let summary = serde_json::to_value(&sample)?;
        assert_eq!(summary["responses"]["payload"], summary["payload_reference"], "result retains the payload field with the approved reference instead of duplicated transactions");
        assert_eq!(summary["payload_reference"], json!({"file":"samples.jsonl","number":7,"selection":"last-row-per-number","pointer":"/responses/payload"}), "reference must define a stable row selector and JSON pointer");
        let row: Value = serde_json::from_str(fs::read_to_string(directory.path().join("samples.jsonl"))?.trim())?;
        assert_eq!(row.pointer("/responses/payload"), Some(&serde_json::to_value(sample.pending.envelope.as_ref().unwrap())?), "JSONL retains exact complete native envelope");
        assert_eq!(sample.responses.get("payload"), sample.payload_reference.as_ref(), "flushing retains only small reference metadata, not a second JSON envelope");
        Ok(())
    }

    /// Streamed hashing matches one-shot hashing across read-buffer boundaries.
    #[test]
    fn streamed_executable_digest_and_cached_error() -> Result<()> {
        let bytes = vec![0x5a; 131_073];
        assert_eq!(RootBenchRunArgs::hash_reader(&mut bytes.as_slice())?, keccak256(&bytes), "streamed digest must cover every byte including tail");
        let cache = std::sync::OnceLock::new();
        let attempts = std::sync::atomic::AtomicUsize::new(0);
        for _ in 0..2 {
            assert!(RootBenchRunArgs::cached_digest(&cache, || { attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst); Err(eyre!("injected read failure")) }).is_err(), "cached failure must stay a failure");
        }
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 1, "cached error must prevent repeated executable reads");
        let success = Arc::new(OnceLock::new());
        let reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut calls = Vec::new();
        for _ in 0..8 {
            let success = success.clone(); let reads = reads.clone();
            calls.push(std::thread::spawn(move || RootBenchRunArgs::cached_digest(&success, || { reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst); Ok(B256::repeat_byte(3)) })));
        }
        for call in calls { assert_eq!(call.join().expect("cache caller must not panic")?, B256::repeat_byte(3), "all parallel callers must share exact digest"); }
        assert_eq!(reads.load(std::sync::atomic::Ordering::SeqCst), 1, "parallel preflight must initialize successful digest once");
        Ok(())
    }

    /// Unwinding Engine work is converted to an active typed failure with retained samples.
    #[tokio::test]
    async fn driver_panic_retains_all_buffered_samples() -> Result<()> {
        let mut engine = MockRootBenchEngine::new();
        engine.expect_fcu().times(1).returning(|_, _| panic!("injected Engine panic"));
        let mut samples = Vec::new();
        let mut start = None;
        let result = RootBenchRunArgs::catch_active(RootBenchSample::drive(&engine, vec![RootBenchBundle::attributes(1, vec![])], RootBenchDrive { genesis:B256::ZERO, warmup:0, origin:Instant::now(), timeout:Duration::from_secs(1) }, &mut samples, &mut start)).await;
        assert_eq!(RootBenchExit::code(&result), 3, "caught harness panic must be incomplete, not refused");
        let directory = tempfile::tempdir()?;
        let mut outcome = result;
        RootBenchSample::flush_all(&mut samples, directory.path(), &mut outcome);
        assert_eq!(fs::read_to_string(directory.path().join("samples.jsonl"))?.lines().count(), 1, "panic must retain the attempted sample");
        let mut samples = vec![RootBenchSample::new(1, false), RootBenchSample::new(2, false)];
        let mut attempts = Vec::new();
        RootBenchSample::flush_all_with(&mut samples, &mut outcome, |sample| {
            attempts.push(sample.number);
            if sample.number == 1 { panic!("injected persistence panic"); }
            sample.persist(directory.path())
        });
        assert_eq!(attempts, vec![1, 2], "persistence panic must not suppress independent later evidence");
        assert_eq!(RootBenchExit::code(&outcome), 3, "caught persistence panic must remain an active incomplete failure");
        Ok(())
    }

    /// The named gas constant and recommended warmup preserve frozen-input compatibility.
    #[test]
    fn named_gas_limit_and_activation_warmup() -> Result<()> {
        assert_eq!(ROOT_BENCH_GAS_LIMIT, 126_000_000, "fixed gas contract must not change");
        let bundle = RootBenchBundle::freeze(4, 1, 2)?;
        assert_eq!(bundle.version, 1, "input version and digest schema stay unchanged");
        assert_eq!(bundle.genesis.gas_limit, ROOT_BENCH_GAS_LIMIT, "genesis uses fixed gas constant");
        assert_eq!(bundle.blocks[0].gas_limit, Some(ROOT_BENCH_GAS_LIMIT), "attributes use fixed gas constant");
        assert_eq!(bundle.blocks[bundle.warmup as usize].payload_attributes.timestamp, 3, "recommended warmup excludes activation timestamp two");
        Ok(())
    }

    /// Phase sums exclude inter-call work; throughput rejects absent/zero denominators.
    #[test]
    fn exact_cycle_and_throughput() {
        let mut sample = RootBenchSample::new(1, false);
        for (phase, start, end) in [("build_fcu", 1, 11), ("get_payload", 20, 40), ("new_payload", 60, 90), ("canonical_fcu", 100, 140)] {
            sample.phases.insert(phase.into(), RootBenchInterval { start_ns:start, end_ns:end });
        }
        sample.finish_cycle();
        sample.accepted_gas = 42_000; sample.successful_receipts = 2;
        assert_eq!(sample.cycle_ns, Some(100), "cycle must sum only four Engine phases");
        assert_eq!(RootBenchSummary::phase(&[sample.clone()], "cycle").p50_ns, Some(100), "summary must use phase sum, not wall span");
        assert_eq!(RootBenchSummary::throughput(&[sample.clone()], Some(10), 2_000_000_010), Some((21_000.0, 1.0)), "units must be gas/s and receipts/s");
        for start in [None, Some(2_000_000_010), Some(2_000_000_011)] {
            assert_eq!(RootBenchSummary::throughput(&[sample.clone()], start, 2_000_000_010), None, "missing/nonpositive wall time is absent");
        }
        sample.warmup = true;
        assert_eq!(RootBenchSummary::throughput(&[sample.clone()], Some(0), 1_000_000_000), None, "warmup is not measurement");
        sample.warmup = false; sample.error = Some("failed".into());
        let mut absent = RootBenchSample::new(2, false); absent.accepted_gas = 42_000;
        assert_eq!(RootBenchSummary::throughput(&[absent], Some(0), 1_000_000_000), None, "missing completed cycle cannot contribute");
        assert_eq!(RootBenchSummary::throughput(&[sample], Some(0), 1_000_000_000), None, "errors are not throughput observations");
    }

    /// Raw input evidence preserves noncanonical JSON bytes exactly.
    #[tokio::test]
    async fn input_bytes_are_verbatim() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let raw = format!(" \n{}\t\n", serde_json::to_string_pretty(&serde_json::to_value(RootBenchBundle::freeze(4, 1, 1)?)?)?).into_bytes();
        let input = directory.path().join("input1.json");
        RootBenchBundle::write_raw_new(&input, &raw)?;
        let output = directory.path().join("trial1");
        let args = RootBenchRunArgs { input, output:output.clone(), executable_manifest:directory.path().join("missing-manifest1.json"), cohort:RootBenchCohort::R, call_timeout_ms:1, run_timeout_ms:10, shutdown_timeout_ms:10 };
        assert!(args.run().await.is_err(), "missing attestation must refuse before node launch");
        assert_eq!(fs::read(output.join("input.json"))?, raw, "production input capture must preserve every byte");
        Ok(())
    }

    /// Typed deadlines survive later filesystem failures; other failures remain incomplete.
    #[test]
    fn exit_categories_and_required_manifest() -> Result<()> {
        for (primary, expected) in [(RootBenchExit::Timeout, 4), (RootBenchExit::Incomplete, 3), (RootBenchExit::Refused, 2)] {
            let mut outcome = Err(eyre::Report::new(primary));
            RootBenchCleanup::retain_error(&mut outcome, Err(eyre!("filesystem failed")));
            assert_eq!(RootBenchExit::code(&outcome), expected, "cleanup must retain primary category");
        }
        assert_eq!(RootBenchExit::code(&Ok(())), 0, "success has code zero");
        assert_eq!(RootBenchExit::code(&Err(eyre!("opaque internal timeout"))), 3, "message text must never infer timeout");
        let mut validation_then_timeout = Err(RootBenchExit::Incomplete.attach(eyre!("validation failed")));
        RootBenchCleanup::retain_error(&mut validation_then_timeout, Err(eyre::Report::new(RootBenchExit::Timeout)));
        RootBenchCleanup::retain_error(&mut validation_then_timeout, Err(eyre!("filesystem failed")));
        assert_eq!(RootBenchExit::code(&validation_then_timeout), 4, "actually observed cleanup deadline takes precedence");
        let parsed = crate::BenchmarkCli::try_parse_from(["base-bench", "root-bench", "run", "--input", "input1.json", "--output", "trial1", "--cohort", "d", "--executable-manifest", "build1.json"])?;
        let Some(crate::BenchmarkCommand::RootBench(args)) = parsed.command else { panic!("parser must select root benchmark"); };
        let RootBenchCommand::Run(args) = args.command else { panic!("parser must select run"); };
        assert_eq!((args.call_timeout_ms, args.run_timeout_ms, args.shutdown_timeout_ms), (30_000, 300_000, 30_000), "deadline defaults must use exact milliseconds");
        assert_eq!(args.executable_manifest, PathBuf::from("build1.json"), "required manifest retains provided path");
        let parsed = RootBenchArgs::try_parse_from(["root-bench", "freeze", "--output", "input1.json"])?;
        let RootBenchCommand::Freeze { blocks, transactions_per_block, warmup, .. } = parsed.command else { panic!("parser must select freeze"); };
        assert_eq!((blocks, transactions_per_block, warmup), (8, 10, 1), "freeze defaults must remain fixed");
        for (blocks, transfers, warmup) in [(3, 1, 1), (10001, 1, 1), (4, 0, 1), (4, 1001, 1), (4, 1, 4)] {
            assert!(RootBenchBundle::freeze(blocks, transfers, warmup).is_err(), "unsupported workload bounds must refuse");
        }
        assert!(crate::BenchmarkCli::try_parse_from(["base-bench", "root-bench", "run", "--input", "input1.json", "--output", "trial1", "--cohort", "r"]).is_err(), "manifest must be required by Clap");
        Ok(())
    }

    /// Aggregate correctness includes warmup and only active D timestamps.
    #[test]
    fn aggregate_active_counts() -> Result<()> {
        let bundle = RootBenchBundle::freeze(4, 1, 1)?;
        assert_eq!(RootBenchCohort::R.active_blocks(&bundle), 4, "R includes every warmup and measured block");
        assert_eq!(RootBenchCohort::D.active_blocks(&bundle), 3, "D includes timestamps 2 through 4");
        for (cohort, count) in [(RootBenchCohort::R, 4), (RootBenchCohort::D, 3)] {
            let raw = format!("reth_base_root_experiment_builder_executions {count}\nreth_base_root_experiment_validator_executions {count}\nreth_base_root_experiment_current_roots_started{{source=\"builder\"}} {count}\nreth_base_root_experiment_current_roots_started{{source=\"validator\"}} {count}\nreth_base_root_experiment_validator_ready_outputs {count}\n");
            cohort.check_accounting("", &raw, cohort.active_blocks(&bundle))?;
            assert!(cohort.check_accounting("", &raw, 1).is_err(), "per-block gate must not pass aggregate observations");
        }
        for cohort in [RootBenchCohort::O, RootBenchCohort::S, RootBenchCohort::R, RootBenchCohort::D] {
            let directory = tempfile::tempdir()?;
            assert!(cohort.launch_config(&bundle, directory.path())?.node_config.prune_config().is_none(), "all cohorts need unpruned history");
        }
        Ok(())
    }

    /// Production driver uses strict ordered calls, retains envelopes and extends accepted parents.
    #[tokio::test]
    async fn engine_driver_order_and_next_parent() -> Result<()> {
        let mut engine = MockRootBenchEngine::new();
        let mut sequence = mockall::Sequence::new();
        let attrs = RootBenchBundle::freeze(4, 1, 1)?.blocks;
        let genesis = B256::repeat_byte(9);
        let mut parent = genesis;
        for (index, attributes) in attrs.iter().take(2).enumerate() {
            let hash = B256::repeat_byte(index as u8 + 1);
            let expected_attrs = attributes.clone();
            engine.expect_fcu().withf(move |state, attrs| state.head_block_hash == parent && state.safe_block_hash == parent && state.finalized_block_hash == genesis && attrs.as_ref() == Some(&expected_attrs)).times(1).in_sequence(&mut sequence).returning(move |_, _| Ok(alloy_rpc_types_engine::ForkchoiceUpdated { payload_status:PayloadStatus::new(PayloadStatusEnum::Valid, Some(parent)), payload_id:Some(alloy_rpc_types_engine::PayloadId::new([1; 8])) }));
            engine.expect_payload().withf(|id| *id == alloy_rpc_types_engine::PayloadId::new([1; 8])).times(1).in_sequence(&mut sequence).returning(move |_| Ok(SelfFixture::envelope(hash)));
            engine.expect_import().withf(move |payload, _| payload.payload_inner.payload_inner.payload_inner.block_hash == hash).times(1).in_sequence(&mut sequence).returning(move |_, _| Ok(PayloadStatus::new(PayloadStatusEnum::Valid, Some(hash))));
            engine.expect_fcu().withf(move |state, attrs| state.head_block_hash == hash && attrs.is_none()).times(1).in_sequence(&mut sequence).returning(move |_, _| Ok(alloy_rpc_types_engine::ForkchoiceUpdated { payload_status:PayloadStatus::new(PayloadStatusEnum::Valid, Some(hash)), payload_id:None }));
            parent = hash;
        }
        let mut samples = Vec::with_capacity(2);
        RootBenchSample::drive(&engine, attrs.into_iter().take(2).collect(), RootBenchDrive { genesis, warmup:1, origin:Instant::now(), timeout:Duration::from_secs(1) }, &mut samples, &mut None).await?;
        assert_eq!(samples.len(), 2, "exactly two blocks must finish");
        assert_eq!(samples[1].parent_hash, B256::repeat_byte(1), "second build must extend first accepted hash");
        assert!(samples.iter().all(|s| s.pending.envelope.is_some() && s.responses.is_empty()), "typed evidence must survive without serialization");
        Ok(())
    }

    /// The actual phase timeout wrapper records raw boundaries and a typed observed deadline.
    #[tokio::test]
    async fn every_phase_deadline_is_typed() -> Result<()> {
        for phase in ["build_fcu", "get_payload", "new_payload", "canonical_fcu"] {
            let mut sample = RootBenchSample::new(1, false);
            let result = sample.call(phase, Instant::now(), Duration::from_millis(1), std::future::pending::<Result<()>>()).await;
            assert_eq!(RootBenchExit::code(&result), 4, "observed {phase} deadline must have exit 4");
            assert_eq!(sample.phases.len(), 1, "deadline records only the attempted phase");
            sample.finish_cycle();
            assert_eq!(sample.cycle_ns, None, "failed phases cannot produce completed cycles");
        }
        Ok(())
    }

    /// Each phase stops at its first transport/status/deadline failure without retry.
    #[tokio::test]
    async fn engine_driver_fails_closed() -> Result<()> {
        for phase in 0..4 {
            for failure in 0..8 {
                if (phase == 1 && failure > 1) || (phase != 0 && failure == 5) { continue; }
                let mut engine = MockRootBenchEngine::new();
                let mut sequence = mockall::Sequence::new();
                let parent = B256::repeat_byte(9);
                let hash = B256::repeat_byte(1);
                for step in 0..=phase {
                    let failed = step == phase;
                    let response_hash = if step == 0 { parent } else { hash };
                    let status = if failed && failure == 2 { PayloadStatus::new(PayloadStatusEnum::Syncing, None) } else if failed && failure == 3 { PayloadStatus::new(PayloadStatusEnum::Valid, None) } else if failed && failure == 6 { PayloadStatus::new(PayloadStatusEnum::Invalid { validation_error:"invalid root".into() }, Some(response_hash)) } else if failed && failure == 7 { PayloadStatus::new(PayloadStatusEnum::Accepted, Some(response_hash)) } else { PayloadStatus::new(PayloadStatusEnum::Valid, Some(if failed && failure == 4 { B256::ZERO } else { response_hash })) };
                    match step {
                        0 | 3 => {
                            engine.expect_fcu().times(1).in_sequence(&mut sequence).returning(move |_, _| {
                                if failed && failure == 0 { return Err(eyre!("injected transport")); }
                                if failed && failure == 1 { return Err(eyre::Report::new(RootBenchExit::Timeout)); }
                                Ok(alloy_rpc_types_engine::ForkchoiceUpdated { payload_status:status.clone(), payload_id:if failed && failure == 5 { None } else { Some(alloy_rpc_types_engine::PayloadId::new([1; 8])) } })
                            });
                        }
                        1 => { engine.expect_payload().times(1).in_sequence(&mut sequence).returning(move |_| if failed { Err(if failure == 1 { eyre::Report::new(RootBenchExit::Timeout) } else { eyre!("injected payload transport") }) } else { Ok(SelfFixture::envelope(hash)) }); }
                        _ => { engine.expect_import().times(1).in_sequence(&mut sequence).returning(move |_, _| if failed && failure < 2 { Err(if failure == 1 { eyre::Report::new(RootBenchExit::Timeout) } else { eyre!("injected import transport") }) } else { Ok(status.clone()) }); }
                    }
                }
                // A missing payload ID matters only at build admission; other phases were filtered.
                let mut samples = Vec::with_capacity(1);
                let result = RootBenchSample::drive(&engine, vec![RootBenchBundle::attributes(1, vec![])], RootBenchDrive { genesis:parent, warmup:0, origin:Instant::now(), timeout:Duration::from_millis(1) }, &mut samples, &mut None).await;
                assert!(result.is_err(), "phase {phase} failure {failure} must stop driver");
                assert_eq!(samples.len(), 1, "failed phase retains exactly one sample");
                if phase > 1 { assert!(samples[0].pending.envelope.is_some(), "later failure retains typed payload"); }
            }
        }
        Ok(())
    }

    /// Activation validates the actual pre-fork root after payload reconstruction.
    #[test]
    fn activation_parent_root_and_payload_identity() -> Result<()> {
        let attrs = RootBenchBundle::attributes(2, vec![]);
        let parent = B256::repeat_byte(3);
        let actual_root = B256::repeat_byte(4);
        let mut block = alloy_consensus::Block::<Bytes> { header:alloy_consensus::Header { parent_hash:parent, number:2, timestamp:2, state_root:actual_root, ..Default::default() }, body:Default::default() };
        RootBenchSample::verify_block(&block, &attrs, parent, 2, RootBenchCohort::D, 2, Some(actual_root))?;
        assert!(RootBenchSample::verify_block(&block, &attrs, parent, 2, RootBenchCohort::D, 2, Some(B256::ZERO)).is_err(), "current/delayed root is not actual pre-F root");
        block.header.parent_hash = B256::ZERO;
        assert!(RootBenchSample::verify_block(&block, &attrs, parent, 2, RootBenchCohort::D, 2, Some(actual_root)).is_err(), "wrong parent rejects");
        block.header.parent_hash = parent; block.header.number = 3;
        assert!(RootBenchSample::verify_block(&block, &attrs, parent, 2, RootBenchCohort::D, 2, Some(actual_root)).is_err(), "wrong number rejects");
        block.header.number = 2;
        block.header.timestamp = 3;
        assert!(RootBenchSample::verify_block(&block, &attrs, parent, 2, RootBenchCohort::D, 2, Some(actual_root)).is_err(), "wrong payload timestamp rejects");
        block.header.timestamp = 2; block.body.transactions.push(Bytes::from_static(b"wrong"));
        assert!(RootBenchSample::verify_block(&block, &attrs, parent, 2, RootBenchCohort::D, 2, Some(actual_root)).is_err(), "changed transaction bytes/order rejects");
        Ok(())
    }

    /// Native envelope fixture needs no provider or root computation.
    pub struct SelfFixture;
    impl SelfFixture {
        /// Constructs a typed payload carrying a chosen advertised hash.
        pub fn envelope(hash: B256) -> base_common_rpc_types_engine::BaseExecutionPayloadEnvelopeV4 {
            let block = alloy_consensus::Block::<TxEnvelope> { header:alloy_consensus::Header { withdrawals_root:Some(B256::ZERO), blob_gas_used:Some(0), excess_blob_gas:Some(0), requests_hash:Some(alloy_eips::eip7685::EMPTY_REQUESTS_HASH), parent_beacon_block_root:Some(B256::ZERO), ..Default::default() }, body:Default::default() };
            let data = ExecutionData::from_block_slow(&block);
            let mut payload = data.payload.as_v4().expect("fixture selects V4").clone();
            payload.payload_inner.payload_inner.payload_inner.block_hash = hash;
            base_common_rpc_types_engine::BaseExecutionPayloadEnvelopeV4 { execution_payload:payload, block_value:U256::ZERO, blobs_bundle:Default::default(), should_override_builder:false, parent_beacon_block_root:B256::ZERO, execution_requests:vec![] }
        }
    }

    /// All captured values and argv tokens are opaque, and launch settings never copy domains.
    #[test]
    fn launch_artifacts_hash_all_environment_and_arguments() -> Result<()> {
        let values: Vec<_> = ["BASE_L1_RPC", "RETH_AUTH_JWTSECRET", "BASE_SIGNER_HEADER", "RETH_FORWARD_ENDPOINT", "BASE_LOG_LEVEL", "RETH_THREADS"].into_iter().enumerate().map(|(index, name)| (name.to_owned(), format!("distinct-raw-value-{index}"))).collect();
        let environment = RootBenchRunArgs::environment_fingerprints(values.clone());
        assert_eq!(environment.len(), 6, "all six captured fields, including benign ones, must be hashed");
        for (name, value) in &values {
            assert_eq!(environment[name], format!("keccak:{}", keccak256(value)), "each captured field must contain only its exact fingerprint");
        }
        // All credential strings and the reserved .invalid domain below are synthetic test data.
        let arguments = ["--rpc=https://user:credential@rpc.invalid", "--jwt-secret", "raw-jwt-value", "/path/with-credential"];
        let argv = RootBenchRunArgs::argument_fingerprints(arguments.map(std::ffi::OsString::from));
        assert_eq!(argv.len(), 4, "every argument token must be retained only as a hash");
        for (hashed, raw) in argv.iter().zip(arguments) { assert_eq!(*hashed, format!("keccak:{}", keccak256(raw)), "flag, separate value and path tokens must all be hashed"); }
        let bundle = RootBenchBundle::freeze(4, 1, 1)?;
        let directory = tempfile::tempdir()?;
        let mut launch = RootBenchCohort::R.launch_config(&bundle, &directory.path().join("raw-datadir-secret"))?;
        launch.standard.rpc.rpc_forwarding_endpoint = Some("https://user:raw-forward-secret@rpc.invalid".parse()?);
        let artifact = json!({"environment":environment,"argv":argv,"config":RootBenchRunArgs::launch_settings(&launch)});
        let serialized = serde_json::to_string(&artifact)?;
        for raw in values.iter().map(|(_, value)| value.as_str()).chain(arguments).chain(["raw-datadir-secret", "raw-forward-secret", "rpc.invalid"]) {
            assert!(!serialized.contains(raw), "serialized launch artifact must not contain any raw captured value, credential or endpoint");
        }
        assert_eq!(artifact["config"]["queue"], 6, "explicit nonsecret queue setting must remain readable");
        assert_eq!(artifact["config"]["persistence_threshold"], 0, "explicit nonsecret persistence setting must remain readable");
        Ok(())
    }

    /// The largest admitted transfer block cannot raise the frozen workload's base fee.
    #[test]
    fn maximum_transfer_block_stays_below_fee_target() -> Result<()> {
        let genesis = RootBenchBundle::transfer_genesis()?;
        let attrs = RootBenchBundle::attributes(1, vec![]);
        let maximum_used_gas = 1000 * 21_000;
        assert_eq!(genesis.gas_limit, attrs.gas_limit.expect("fixed gas limit"), "genesis and payload must use the same gas limit");
        assert!(maximum_used_gas <= genesis.gas_limit / 6, "the largest admitted workload must not exceed the elasticity-six gas target and raise its fee");
        Ok(())
    }

    /// Serialization and persistence errors cannot prevent graceful or actual runtime shutdown.
    #[tokio::test]
    async fn evidence_failures_still_shutdown_runtime() -> Result<()> {
        for serialization in [true, false] {
            let runtime = std::thread::spawn(|| RuntimeBuilder::new(RuntimeConfig::default()).build()).join().expect("runtime construction must not panic")?;
            let critical = runtime.take_task_manager_handle();
            let observer = runtime.clone();
            let (signalled, observed) = tokio::sync::oneshot::channel();
            runtime.spawn_with_graceful_shutdown_signal(move |shutdown| async move { let guard = shutdown.await; signalled.send(()).expect("test must observe shutdown"); drop(guard); });
            let directory = tempfile::tempdir()?;
            let failure = if serialization {
                RootBenchBundle::write_new(&directory.path().join("bad1.json"), &BTreeMap::from([((1u8, 2u8), 3u8)]))
            } else {
                RootBenchSample::new(1, false).persist(&directory.path().join("missing"))
            };
            assert!(failure.is_err(), "fixture must actually fail serialization or persistence");
            let mut outcome = Err(eyre!("primary trial failure"));
            RootBenchCleanup::retain_error(&mut outcome, failure);
            let mut sample = RootBenchSample::new(1, false);
            let output = if serialization { directory.path().to_path_buf() } else { directory.path().join("missing") };
            let drain = RootBenchCleanup { runtime, critical, critical_result:None, manager:None, manager_required:false, receipt_tasks:vec![], timeout:Duration::from_secs(2) }.finish(Some(&mut sample), &output, &mut outcome).await;
            observed.await?;
            assert_eq!(drain["guards_completed"], true, "evidence failure must not skip graceful shutdown");
            assert_eq!(drain["receipt_reads_completed"], true, "empty read set must report exact completion");
            assert!(outcome.unwrap_err().to_string().starts_with("primary trial failure; subsequent failure:"), "cleanup must preserve the primary error and append the evidence failure");
            assert!(observer.handle().spawn(async {}).await.expect_err("owned runtime must actually be shut down").is_cancelled(), "post-cleanup work must be cancelled, not run on a leaked runtime");
        }
        Ok(())
    }

    /// Provider and monitor JoinErrors remain failures without skipping either shutdown phase.
    #[tokio::test]
    async fn join_failures_still_shutdown_runtime() -> Result<()> {
        for provider_panic in [true, false] {
            let runtime = std::thread::spawn(|| RuntimeBuilder::new(RuntimeConfig::default()).build()).join().expect("runtime construction must not panic")?;
            let monitor = runtime.take_task_manager_handle();
            let observer = runtime.clone();
            let (signalled, observed) = tokio::sync::oneshot::channel();
            runtime.spawn_with_graceful_shutdown_signal(move |shutdown| async move { let guard = shutdown.await; signalled.send(()).expect("test must observe shutdown"); drop(guard); });
            let (critical, receipt_tasks) = if provider_panic {
                let task = runtime.spawn_blocking(|| -> Result<u64> { panic!("injected provider panic") });
                while !task.is_finished() { tokio::task::yield_now().await; }
                (monitor, vec![task])
            } else {
                let task = runtime.handle().spawn(async { std::future::pending::<()>().await; Ok(()) });
                task.abort();
                (Some(task), vec![])
            };
            let mut outcome = Ok(());
            let drain = RootBenchCleanup { runtime, critical, critical_result:None, manager:None, manager_required:false, receipt_tasks, timeout:Duration::from_secs(2) }.finish(None, Path::new("."), &mut outcome).await;
            observed.await?;
            assert_eq!(drain["guards_completed"], true, "join failures must not skip graceful shutdown");
            let error = outcome.expect_err("join failure must fail the trial").to_string();
            assert!(error.contains(if provider_panic { "provider read join failed" } else { "critical task failure" }), "the actual provider or critical JoinError must be retained");
            assert!(observer.handle().spawn(async {}).await.expect_err("owned runtime must actually be shut down").is_cancelled(), "join failure must not leak the owned runtime");
        }
        Ok(())
    }

    /// An unavailable critical monitor still drains graceful work and destroys the owned runtime.
    #[tokio::test]
    async fn missing_monitor_still_shutdown_runtime() -> Result<()> {
        let runtime = std::thread::spawn(|| RuntimeBuilder::new(RuntimeConfig::default()).build()).join().expect("runtime construction must not panic")?;
        let observer = runtime.clone();
        let (signalled, observed) = tokio::sync::oneshot::channel();
        runtime.spawn_with_graceful_shutdown_signal(move |shutdown| async move { let guard = shutdown.await; signalled.send(()).expect("test must observe shutdown"); drop(guard); });
        let mut outcome = Err(eyre!("runtime critical-task monitor unavailable"));
        let drain = RootBenchCleanup { runtime, critical:None, critical_result:None, manager:None, manager_required:false, receipt_tasks:vec![], timeout:Duration::from_secs(2) }.finish(None, Path::new("."), &mut outcome).await;
        observed.await?;
        assert_eq!(drain["guards_completed"], true, "missing monitor must not skip graceful shutdown");
        assert!(outcome.unwrap_err().to_string().contains("runtime critical-task monitor unavailable"), "missing monitor must remain the primary failure");
        assert!(observer.handle().spawn(async {}).await.expect_err("owned runtime must actually be shut down").is_cancelled(), "missing monitor must not leak the owned runtime");
        Ok(())
    }

    /// Harness capture and node launch share Reth's recorder and exported prefix.
    #[test]
    fn shared_reth_recorder_exports_root_metrics() -> Result<()> {
        let recorder = reth_node_metrics::recorder::install_prometheus_recorder();
        let handle = recorder.handle().clone();
        assert!(std::ptr::eq(recorder, reth_node_metrics::recorder::install_prometheus_recorder()), "node launch must reuse the harness recorder without a second installation");
        let key = "reth_base_root_experiment_payloads_sealed";
        let before = RootBenchCohort::counter(&RootBenchRunArgs::root_metrics(&handle), key)?;
        base_execution_payload_builder::PostStateRootMetrics::payloads_sealed().increment(1);
        let captured = RootBenchRunArgs::root_metrics(&handle);
        assert_eq!(RootBenchCohort::counter(&captured, key)?, before + 1.0, "capture must retain the actual Reth-prefixed production metric");
        assert!(captured.contains("# TYPE reth_base_root_experiment_payloads_sealed counter"), "capture must retain metric family metadata");
        Ok(())
    }

    /// Receipt work remains observable on the owned runtime after a caller timeout.
    #[tokio::test]
    async fn receipt_timeout_retains_owned_work() -> Result<()> {
        let runtime = std::thread::spawn(|| RuntimeBuilder::new(RuntimeConfig::default()).build()).join().expect("runtime construction must not panic")?;
        let (release, blocked) = std::sync::mpsc::channel();
        let (started, running) = tokio::sync::oneshot::channel();
        let mut task = runtime.spawn_blocking(move || { started.send(()).expect("caller must await start"); blocked.recv().expect("caller must release read"); });
        running.await?;
        assert!(tokio::time::timeout(Duration::from_millis(1), &mut task).await.is_err(), "blocked read must exceed caller deadline");
        assert!(!task.is_finished(), "timed-out read must remain observable, not claim completion");
        release.send(())?;
        tokio::time::timeout(Duration::from_secs(2), &mut task).await??;
        assert!(task.is_finished(), "released read must complete before successful drain");
        tokio::task::spawn_blocking(move || runtime.shutdown_timeout(Duration::from_secs(2))).await?;
        Ok(())
    }

    /// Ecotone and Isthmus header fields must survive native envelope reconstruction.
    #[test]
    fn native_payload_sidecars_and_advertised_hash() -> Result<()> {
        let block = alloy_consensus::Block::<TxEnvelope> { header: alloy_consensus::Header { parent_beacon_block_root:Some(B256::repeat_byte(7)), requests_hash:Some(alloy_eips::eip7685::EMPTY_REQUESTS_HASH), withdrawals_root:Some(B256::repeat_byte(8)), blob_gas_used:Some(0), excess_blob_gas:Some(0), base_fee_per_gas:Some(1), ..Default::default() }, body:Default::default() };
        let data = base_common_rpc_types_engine::ExecutionData::from_block_slow(&block);
        let payload = data.payload.as_v4().expect("fixture must select V4").clone();
        let rebuilt = RootBenchSample::payload_block(payload.clone(), B256::repeat_byte(7))?;
        assert_eq!(rebuilt.header.hash_slow(), block.header.hash_slow(), "full fork header must match advertised native hash");
        assert_eq!(rebuilt.header.requests_hash, block.header.requests_hash, "Isthmus requests hash must survive");
        assert!(RootBenchSample::payload_block(payload, B256::ZERO).is_err(), "wrong sidecar must fail advertised hash check");
        Ok(())
    }

    /// Native receipts retain exact cardinality, execution status and cumulative gas order.
    #[test]
    fn native_receipt_verification() -> Result<()> {
        let receipt = |gas: u64, success: bool| base_common_consensus::BaseReceipt::Eip1559(alloy_consensus::Receipt { status:success.into(), cumulative_gas_used:gas, logs:vec![] });
        RootBenchSample::verify_receipts(&[receipt(21_000, true), receipt(42_000, true)], 2, 42_000)?;
        assert!(RootBenchSample::verify_receipts(&[receipt(21_000, true)], 2, 42_000).is_err(), "missing receipt must reject");
        assert!(RootBenchSample::verify_receipts(&[receipt(21_000, false)], 1, 21_000).is_err(), "failed execution must reject");
        assert!(RootBenchSample::verify_receipts(&[receipt(42_000, true), receipt(21_000, true)], 2, 42_000).is_err(), "wrong cumulative gas order must reject");
        assert!(RootBenchSample::verify_receipts(&[receipt(21_000, true)], 1, 42_000).is_err(), "block gas mismatch must reject");
        Ok(())
    }

    /// Guard completion alone cannot certify a retained, timed-out or panicked worker.
    #[test]
    fn drain_requires_healthy_joined_worker() -> Result<()> {
        let manager = Arc::new(PostStateRootManager::new(PostStateRootLimits { timeout:Duration::from_millis(5), ..Default::default() })?);
        assert!(RootBenchRunArgs::verify_worker(Some(&manager), true).is_err(), "live idle worker is not joined");
        let (release, blocked) = std::sync::mpsc::channel();
        let (started, running) = std::sync::mpsc::channel();
        let completion = manager.enqueue(Box::new(move || { started.send(()).unwrap(); blocked.recv().unwrap(); Ok(base_execution_payload_builder::PostStateRootData::new(B256::ZERO, &Default::default(), Default::default())) }))?;
        running.recv_timeout(Duration::from_secs(2))?;
        assert!(manager.drain().is_err(), "blocked worker must time out");
        assert!(RootBenchRunArgs::verify_worker(Some(&manager), true).is_err(), "released graceful guard must not hide drain timeout");
        release.send(())?; let _ = completion.wait();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !manager.worker.lock().unwrap().as_ref().unwrap().is_finished() {
            ensure!(Instant::now() < deadline, "released provider job must finish before cleanup");
            std::thread::yield_now();
        }
        manager.drain()?;
        assert!(RootBenchRunArgs::verify_worker(Some(&manager), true).is_err(), "late join must not clear terminal failure");
        let manager = Arc::new(PostStateRootManager::new(PostStateRootLimits::default())?);
        let _ = manager.enqueue(Box::new(|| panic!("injected root failure")))?.wait(); manager.drain()?;
        assert!(RootBenchRunArgs::verify_worker(Some(&manager), true).is_err(), "worker panic must remain failure after join");
        let manager = Arc::new(PostStateRootManager::new(PostStateRootLimits::default())?); manager.drain()?;
        RootBenchRunArgs::verify_worker(Some(&manager), true)?;
        RootBenchRunArgs::verify_worker(None, false)?;
        assert!(RootBenchRunArgs::verify_worker(None, true).is_err(), "active cohort needs observed manager");
        Ok(())
    }

    /// A real critical runtime panic is consumed as a failure rather than ignored at shutdown.
    #[tokio::test]
    async fn critical_task_panic_is_consumed() -> Result<()> {
        let runtime = std::thread::spawn(|| RuntimeBuilder::new(RuntimeConfig::default()).build()).join().unwrap()?;
        let critical = runtime.take_task_manager_handle().expect("test owns critical monitor");
        runtime.spawn_critical_task("root-bench-injected-failure", async { panic!("injected critical failure"); });
        let observed = tokio::time::timeout(Duration::from_secs(2), critical).await??;
        assert!(observed.is_err(), "critical panic must reach the result consumed by the trial failure branch");
        tokio::task::spawn_blocking(move || runtime.shutdown_timeout(Duration::from_secs(2))).await?;
        Ok(())
    }

    /// Every later transport failure retains typed earlier responses without early serialization.
    #[tokio::test]
    async fn preceding_engine_evidence_survives_failure() -> Result<()> {
        for (failed_phase, timeout) in [("get_payload", false), ("new_payload", false), ("canonical_fcu", false), ("get_payload", true), ("new_payload", true), ("canonical_fcu", true)] {
            let directory = tempfile::tempdir()?;
            let mut sample = RootBenchSample::new(1, false);
            sample.pending.build_fcu = Some(alloy_rpc_types_engine::ForkchoiceUpdated { payload_status:PayloadStatus::new(PayloadStatusEnum::Valid, Some(B256::ZERO)), payload_id:Some(alloy_rpc_types_engine::PayloadId::new([1; 8])) });
            if failed_phase == "canonical_fcu" { sample.pending.new_payload = Some(PayloadStatus::new(PayloadStatusEnum::Valid, Some(B256::ZERO))); }
            assert!(sample.responses.is_empty(), "successful Engine sequence must not serialize responses");
            let failed = if timeout { sample.call(failed_phase, Instant::now(), Duration::from_millis(1), std::future::pending::<Result<()>>()).await } else { sample.call(failed_phase, Instant::now(), Duration::from_secs(1), std::future::ready(Err::<(), _>(eyre!("injected transport failure")))).await };
            sample.flush_pending()?; sample.persist(directory.path())?;
            assert!(failed.is_err(), "later transport failure or timeout must propagate");
            let saved: Value = serde_json::from_str(fs::read_to_string(directory.path().join("samples.jsonl"))?.trim())?;
            assert_eq!(saved["responses"]["build_fcu"]["payloadId"], "0x0101010101010101", "build payload ID must survive every later failure");
            if failed_phase == "canonical_fcu" { assert_eq!(saved["responses"]["new_payload"]["status"], "VALID", "preceding import response must survive canonical failure"); }
        }
        Ok(())
    }

    /// Real missing and malformed manifest files refuse before execution runtime construction.
    #[tokio::test]
    async fn missing_and_invalid_manifest_files_refuse() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let input = directory.path().join("bundle1.json");
        RootBenchBundle::write_new(&input, &RootBenchBundle::freeze(4, 1, 1)?)?;
        for (index, contents) in [None, Some(b"not JSON".as_slice()), Some(b"{}".as_slice())].into_iter().enumerate() {
            let manifest = directory.path().join(format!("manifest{index}.json"));
            if let Some(bytes) = contents { RootBenchBundle::write_raw_new(&manifest, bytes)?; }
            let output = directory.path().join(format!("trial{index}"));
            let args = RootBenchRunArgs { input:input.clone(), output:output.clone(), executable_manifest:manifest, cohort:RootBenchCohort::R, call_timeout_ms:1, run_timeout_ms:10, shutdown_timeout_ms:10 };
            let outcome = args.run().await;
            assert_eq!(RootBenchExit::code(&outcome), 2, "missing/malformed manifest must refuse, never become an active failure");
            let report: Value = serde_json::from_slice(&fs::read(output.join("result.json"))?)?;
            assert_eq!(report["schema_version"], 1, "refusal result must carry additive artifact schema version");
            assert_eq!(report["complete"], false, "manifest refusal cannot certify success");
            assert!(!output.join("runtime.json").exists(), "manifest checks precede runtime construction");
        }
        Ok(())
    }

    /// Runtime checkout identity is never substituted for external executable provenance.
    #[test]
    fn executable_manifest_required_and_bound() -> Result<()> {
        let digest = B256::repeat_byte(1);
        assert!(RootBenchRunArgs::verify_manifest(None, digest).is_err(), "unverified external provenance must refuse measurement success");
        let mut manifest = json!({"executable_keccak":digest,"source_commit":"1".repeat(40),"source_tree":"2".repeat(40),"cargo_lock_keccak":B256::repeat_byte(3),"reth_pin_keccak":B256::repeat_byte(4),"clean_source":true});
        RootBenchRunArgs::verify_manifest(Some(&manifest), digest)?;
        assert!(RootBenchRunArgs::verify_manifest(Some(&manifest), B256::ZERO).is_err(), "manifest for a different executable must reject");
        manifest["clean_source"] = json!(false);
        assert!(RootBenchRunArgs::verify_manifest(Some(&manifest), digest).is_err(), "dirty source cannot claim attested provenance");
        Ok(())
    }

    /// The new nested command is opt-in and leaves the old no-command default unchanged.
    #[test]
    fn command_is_opt_in() -> Result<()> {
        assert!(crate::BenchmarkCli::try_parse_from(["base-bench"])?.command.is_none(), "default must still select local mode");
        let command = crate::BenchmarkCli::try_parse_from(["base-bench", "root-bench", "freeze", "--output", "workload1.json"])?;
        assert!(matches!(command.command, Some(crate::BenchmarkCommand::RootBench(_))), "dedicated command must dispatch without CL arguments");
        Ok(())
    }

    /// Frozen bytes, signing identity, chain, nonce and digest are validated before launch.
    #[test]
    fn frozen_workload_roundtrip_and_tamper() -> Result<()> {
        let bundle = RootBenchBundle::freeze(4, 2, 1)?;
        let again = RootBenchBundle::freeze(4, 2, 1)?;
        assert_eq!(serde_json::to_vec(&bundle)?, serde_json::to_vec(&again)?, "freezing identical inputs must preserve exact signed bytes and order");
        bundle.validate()?;
        let first = TxEnvelope::decode_2718_exact(&bundle.blocks[0].transactions.as_ref().unwrap()[0])?;
        ensure!(first.chain_id() == Some(900) && first.nonce() == 0, "frozen signed bytes must encode chain 900 and nonce zero");
        let mut changed = bundle.clone();
        changed.blocks[0].transactions.as_mut().unwrap().swap(0, 1);
        assert!(changed.validate().is_err(), "reordered signed transactions must fail digest validation");
        changed.digest = changed.compute_digest()?;
        assert!(changed.validate().is_err(), "recomputed digest must not hide a nonce-order violation");
        let mut changed = bundle.clone();
        changed.chain_id = 8453;
        changed.digest = changed.compute_digest()?;
        assert!(changed.validate().is_err(), "production chain must fail before launch");
        assert_eq!(bundle.blocks.len(), 4, "freeze must retain every block");
        assert_eq!(bundle.blocks[0].transactions.as_ref().unwrap().len(), 2, "each block must be nonempty");
        Ok(())
    }

    /// Only D installs a deferred fork and all cohorts use the admitted disposable runner.
    #[test]
    fn cohort_launch_configuration() -> Result<()> {
        let bundle = RootBenchBundle::freeze(4, 1, 1)?;
        let dir = tempfile::tempdir()?;
        for cohort in [RootBenchCohort::O, RootBenchCohort::S, RootBenchCohort::R, RootBenchCohort::D] {
            let config = cohort.launch_config(&bundle, dir.path())?;
            assert_eq!(config.node_config.chain.validate_deferred_state_root_schedule()?, (cohort == RootBenchCohort::D).then_some(2), "only D schedules the fork");
            assert!(config.standard.rpc.rollup_args.deferred_state_root_disposable, "every cohort must select the extension-free admitted runner");
            assert_eq!(config.standard.rpc.rollup_args.root_experiment_queue, 6, "queue must match across arms");
            assert_eq!(config.standard.rpc.rollup_args.root_experiment_ready, 128, "retention must match across arms");
            assert_eq!(config.node_config.debug.invalid_block_hook, None, "witness hook must be explicitly unconfigured");
        }
        Ok(())
    }

    /// Exact nearest-rank values exclude warmup and missing phase samples.
    #[test]
    fn percentiles_and_warmup() {
        assert_eq!(RootBenchSummary::from_samples(&[]), RootBenchSummary::default(), "empty summary must have no fake zero observations");
        let summary = RootBenchSummary::from_samples(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10]);
        assert_eq!((summary.count, summary.p50_ns, summary.p90_ns, summary.p95_ns, summary.p99_ns), (10, Some(5), Some(9), Some(10), Some(10)), "nearest rank must include exact sample count");
        let mut rows = vec![RootBenchSample::new(1, true), RootBenchSample::new(2, false), RootBenchSample::new(3, false)];
        rows[0].phases.insert("cycle".into(), RootBenchInterval { start_ns:0, end_ns:999 });
        rows[1].cycle_ns = Some(10);
        rows[2].error = Some("failed".into());
        rows[2].phases.insert("cycle".into(), RootBenchInterval { start_ns:0, end_ns:500 });
        assert_eq!(RootBenchSummary::phase(&rows, "cycle"), RootBenchSummary::from_samples(&[10]), "warmup and failures must not enter any percentile or count");
        assert_eq!(RootBenchSummary::from_samples(&[42]).p99_ns, Some(42), "singleton p99 must equal its only sample");
        assert_eq!(RootBenchSummary::phase(&rows, "missing").count, 0, "missing samples must never count as zero");
    }

    /// Failed status and wrong identity cannot be mistaken for a fresh valid import.
    #[test]
    fn status_and_independent_accounting() -> Result<()> {
        let hash = B256::repeat_byte(1);
        let valid = PayloadStatus::new(PayloadStatusEnum::Valid, Some(hash));
        RootBenchSample::require_valid(&valid, hash)?;
        assert!(RootBenchSample::require_valid(&valid, B256::ZERO).is_err(), "VALID with wrong latestValidHash must reject");
        assert!(RootBenchSample::require_valid(&PayloadStatus::new(PayloadStatusEnum::Syncing, None), hash).is_err(), "SYNCING must reject");
        assert!(RootBenchCohort::R.check_accounting("", "", 1).is_err(), "AlreadySeen VALID without actual independent execution must reject");
        let metrics = "reth_base_root_experiment_builder_executions 1\nreth_base_root_experiment_validator_executions 1\nreth_base_root_experiment_current_roots_started{source=\"builder\"} 1\nreth_base_root_experiment_current_roots_started{source=\"validator\"} 1\nreth_base_root_experiment_validator_ready_outputs 1\n";
        RootBenchCohort::R.check_accounting("", metrics, 1)?;
        assert!(RootBenchCohort::D.check_accounting("", &format!("{metrics}reth_base_root_experiment_trusted_insertions 1\n"), 1).is_err(), "trusted insertion must invalidate R/D evidence");
        Ok(())
    }

    /// Output allocation is create-new and preserves existing evidence on failure.
    #[test]
    fn outputs_never_overwrite() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("bundle1.json");
        RootBenchBundle::write_new(&path, &serde_json::json!({"original": true}))?;
        assert!(RootBenchBundle::write_new(&path, &serde_json::json!({"original": false})).is_err(), "existing evidence must reject replacement");
        assert_eq!(std::fs::read_to_string(path)?, "{\n  \"original\": true\n}", "failed write must preserve exact original bytes");
        assert!(RootBenchArgs::try_parse_from(["root-bench", "run"]).is_err(), "run must require immutable input and fresh output");
        Ok(())
    }

    /// Deadlines retain failed phase samples, and pre-launch errors retain an incomplete result.
    #[tokio::test]
    async fn timeout_and_partial_artifacts() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let mut sample = RootBenchSample::new(1, false);
        let failed = sample.call("new_payload", Instant::now(), Duration::from_millis(1), std::future::pending::<Result<()>>()).await;
        sample.flush_pending()?; sample.persist(directory.path())?;
        assert!(failed.is_err(), "a never-completing Engine call must hit its deadline without retry");
        assert!(sample.error.as_ref().unwrap().contains("timed out"), "timeout must remain an explicit failure");
        assert_eq!(RootBenchSummary::phase(&[sample], "new_payload").count, 0, "timeout must not count as a successful latency");
        assert_eq!(fs::read_to_string(directory.path().join("samples.jsonl"))?.lines().count(), 1, "exactly one failed phase record must survive");
        let input = directory.path().join("bad1.json");
        RootBenchBundle::write_new(&input, &json!({}))?;
        let output = directory.path().join("trial1");
        let args = RootBenchRunArgs { input, output:output.clone(), executable_manifest:directory.path().join("missing-manifest1.json"), cohort:RootBenchCohort::R, call_timeout_ms:1, run_timeout_ms:10, shutdown_timeout_ms:10 };
        assert!(args.run().await.is_err(), "invalid frozen input must fail before launch");
        let original = fs::read(output.join("result.json"))?;
        let report: Value = serde_json::from_slice(&original)?;
        assert_eq!(report["complete"], false, "pre-launch failure cannot claim completion");
        assert!(!output.join("datadir1").exists(), "invalid input must never create writable node state");
        assert!(fs::create_dir(&output).is_err(), "a failed trial's numbered output cannot be reused");
        assert_eq!(fs::read(output.join("result.json"))?, original, "refused reuse must preserve failure bytes");
        Ok(())
    }

    /// Parallel preflight calls must retain their own validation error, not lock contention.
    #[test]
    fn parallel_preflight_preserves_validation_errors() -> Result<()> {
        let barrier = Arc::new(std::sync::Barrier::new(8));
        let mut calls = Vec::new();
        for index in 0..8 {
            let barrier = barrier.clone();
            calls.push(std::thread::spawn(move || -> Result<()> {
                let directory = tempfile::tempdir()?;
                let input = directory.path().join("bundle1.json");
                let valid = index % 2 == 0;
                if valid { RootBenchBundle::write_new(&input, &RootBenchBundle::freeze(4, 1, 1)?)?; }
                else { RootBenchBundle::write_new(&input, &json!({}))?; }
                let output = directory.path().join("trial1");
                let args = RootBenchRunArgs { input, output:output.clone(), executable_manifest:directory.path().join("missing-manifest1.json"), cohort:RootBenchCohort::R, call_timeout_ms:1, run_timeout_ms:10, shutdown_timeout_ms:10 };
                let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
                barrier.wait();
                let error = runtime.block_on(args.run()).expect_err("every preflight must reject without launching").to_string();
                assert!(!error.contains("only one root trial"), "preflight error must not be replaced by another call's trial lock: {error}");
                if valid { assert!(error.contains("external executable provenance unverified"), "valid input must retain the exact provenance failure: {error}"); }
                else { assert!(error.contains("missing field"), "invalid input must retain its schema failure: {error}"); }
                let report: Value = serde_json::from_slice(&fs::read(output.join("result.json"))?)?;
                assert_eq!(report["complete"], false, "parallel preflight must never claim successful completion");
                Ok(())
            }));
        }
        for call in calls { call.join().expect("preflight thread must not panic")?; }
        Ok(())
    }

    /// An otherwise valid workload cannot launch under runtime-checkout-only provenance.
    #[tokio::test]
    async fn unverified_executable_refuses_launch() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let input = directory.path().join("bundle1.json");
        RootBenchBundle::write_new(&input, &RootBenchBundle::freeze(4, 1, 1)?)?;
        let output = directory.path().join("trial1");
        let args = RootBenchRunArgs { input, output:output.clone(), executable_manifest:directory.path().join("missing-manifest1.json"), cohort:RootBenchCohort::R, call_timeout_ms:1, run_timeout_ms:10, shutdown_timeout_ms:10 };
        let error = args.run().await.expect_err("runtime checkout must not authorize measurement");
        assert!(error.to_string().contains("external executable provenance unverified"), "missing attestation must be the explicit refusal reason");
        let report: Value = serde_json::from_slice(&fs::read(output.join("result.json"))?)?;
        assert_eq!(report["complete"], false, "unverified executable must remain incomplete");
        assert!(!output.join("runtime.json").exists(), "provenance refusal must precede runtime construction");
        Ok(())
    }

    /// Actual CLI-derived typed admission builds nonempty R/D through F-1/F/F+1 without sleeps.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires supported Linux, isolated process and external manifest for the test executable"]
    async fn admitted_nonempty_r_d_engine_trial() -> Result<()> {
        for cohort in [RootBenchCohort::R, RootBenchCohort::D] {
            let dir = tempfile::tempdir()?;
            let input = dir.path().join("bundle1.json");
            RootBenchBundle::write_new(&input, &RootBenchBundle::freeze(4, 2, 1)?)?;
            let executable_manifest = PathBuf::from(std::env::var("ROOT_BENCH_EXECUTABLE_MANIFEST").map_err(|_| eyre!("supported-host integration requires externally supplied manifest for this test executable"))?);
            let args = RootBenchRunArgs { input, output: dir.path().join("trial1"), executable_manifest, cohort, call_timeout_ms: 30_000, run_timeout_ms: 180_000, shutdown_timeout_ms: 30_000 };
            args.run().await?;
            let result: serde_json::Value = serde_json::from_slice(&std::fs::read(dir.path().join("trial1/result.json"))?)?;
            assert_eq!(result["complete"], true, "actual admitted R/D trial must drain and succeed");
            assert_eq!(result["samples"].as_array().unwrap().len(), 4, "all activation boundary blocks must be recorded");
            assert_eq!(result["summaries"]["cycle"]["count"], 3, "one warmup must be excluded");
            assert_eq!(result["schema_version"], 1, "active result must use additive artifact schema version");
            assert_eq!(result["loop_start_ns"], result["samples"][1]["phases"]["build_fcu"]["start_ns"], "saturation starts at first measured Engine phase");
            assert_eq!(result["loop_end_ns"], result["samples"][3]["phases"]["canonical_fcu"]["end_ns"], "saturation ends at final measured canonical response");
            let window = result["loop_end_ns"].as_u64().unwrap() - result["loop_start_ns"].as_u64().unwrap();
            assert_eq!(result["engine_saturated"], json!({"window_ns":window,"measured_blocks":3,"gas_used":126_000,"transactions":6,"gas_per_second":126_000.0 * 1_000_000_000.0 / window as f64,"transactions_per_second":6.0 * 1_000_000_000.0 / window as f64}), "real admitted R/D must use exactly the measured verified totals over raw wall endpoints");
            let rows: Vec<Value> = fs::read_to_string(dir.path().join("trial1/samples.jsonl"))?.lines().map(serde_json::from_str).collect::<std::result::Result<_, _>>()?;
            for summary in result["samples"].as_array().unwrap() {
                let row = rows.iter().rev().find(|row| row["number"] == summary["payload_reference"]["number"]).unwrap();
                assert_eq!(summary["responses"]["payload"], summary["payload_reference"], "result payload keeps its field name as an explicit reference");
                assert_eq!(row["responses"]["payload"]["executionPayload"]["blockHash"], summary["block_hash"], "reference must resolve the exact accepted native payload");
            }
            for sample in result["samples"].as_array().unwrap() {
                assert_eq!(sample["successful_receipts"], 2, "both native receipts must be independently checked at each boundary block");
                assert_eq!(sample["accepted_gas"], 42_000, "both transfers must contribute exact canonical gas");
            }
            let launch: Value = serde_json::from_slice(&fs::read(dir.path().join("trial1/launch.json"))?)?;
            assert_eq!(launch["receipt_source"], "canonical-execution-provider-receipts_by_block", "EL-only fixture must not depend on L1-info RPC enrichment");
            assert_eq!(launch["account_source"], "canonical-execution-provider-state_by_block_hash/basic_account", "nonce and balance must use the accepted-hash provider, not unavailable account RPCs");
            let drain: Value = serde_json::from_slice(&fs::read(dir.path().join("trial1/drain.json"))?)?;
            assert_eq!(drain["manager_observed"], true, "active R/D trials must observe the actual shared manager");
            assert_eq!(drain["worker_joined_healthy"], true, "guard completion alone must not establish successful drain");
        }
        Ok(())
    }
}
