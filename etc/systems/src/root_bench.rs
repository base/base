//! Isolated serialized Engine build/import benchmark.

use std::{collections::BTreeMap, fs::{self, OpenOptions}, future::Future, io::Write, path::{Path, PathBuf}, sync::{Arc, OnceLock}, time::{Duration, Instant}};
use alloy_consensus::{Transaction, TxEnvelope, TxReceipt, transaction::SignerRecoverable};
use alloy_eips::Decodable2718;
use alloy_genesis::Genesis;
use alloy_primitives::{Address, B64, B256, Bytes, U256, keccak256};
use alloy_provider::{Provider, RootProvider};
use alloy_rpc_client::RpcClient;
use alloy_rpc_types_engine::{ForkchoiceState, PayloadAttributes, PayloadStatus, PayloadStatusEnum};
use alloy_signer_local::PrivateKeySigner;
use base_common_consensus::JovianExtraData;
use base_common_network::{Base, BaseEngineApi};
use base_common_rpc_types_engine::{BasePayloadAttributes, BaseExecutionPayloadV4, ExecutionData};
use base_execution_payload_builder::PostStateRootManager;
use base_consensus_engine::InMemoryTransport;
use base_execution_chainspec::BaseChainSpec;
use base_execution_cli::{ExecutionNodeArgs, ExecutionNodeLaunchConfig};
use base_load_tests::{Fees, PreparedTransaction, SubmissionPipeline, SubmitCohort};
use clap::{Args, Parser, Subcommand, ValueEnum};
use eyre::{Result, ensure, eyre};
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use reth_cli_runner::CliContext;
use reth_provider::ReceiptProvider;
use reth_tasks::{RuntimeBuilder, RuntimeConfig};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

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
    pub executable_manifest: Option<PathBuf>,
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
    /// Unserialized responses from the current Engine sequence.
    #[serde(skip)]
    pub pending: RootBenchPending,
    /// Raw Engine responses and payload ID.
    pub responses: BTreeMap<String, Value>,
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

impl RootBenchArgs {
    /// Dispatches preparation separately from execution.
    pub async fn run(self) -> Result<()> {
        match self.command {
            RootBenchCommand::Freeze { output, blocks, transactions_per_block, warmup } => {
                RootBenchBundle::numbered(&output)?;
                RootBenchBundle::write_new(&output, &RootBenchBundle::freeze(blocks, transactions_per_block, warmup)?)
            }
            RootBenchCommand::Run(args) => args.run().await,
        }
    }
}

impl RootBenchBundle {
    /// Signs deterministic state-changing transfers outside the timed run.
    pub fn freeze(blocks: u64, transactions: u64, warmup: u64) -> Result<Self> {
        ensure!(blocks >= 4 && blocks <= 10_000 && transactions > 0 && transactions <= 1000 && warmup < blocks, "require 4..10000 blocks, 1..1000 transfers and warmup < blocks");
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
            "timestamp":"0x0","gasLimit":"0x1c9c380","difficulty":"0x0","baseFeePerGas":"0x3b9aca00","alloc":{sender.to_string():{"balance":"0xd3c21bcecceda1000000"}}
        }))?;
        let params = BaseChainSpec::from_genesis(genesis.clone()).base_fee_params_at_timestamp(0);
        genesis.extra_data = JovianExtraData::encode(B64::ZERO, params, 0)?;
        Ok(genesis)
    }

    /// Constructs exact fixed block attributes while retaining existing signed bytes unchanged.
    pub fn attributes(timestamp: u64, transactions: Vec<Bytes>) -> BasePayloadAttributes {
        BasePayloadAttributes {
            payload_attributes: PayloadAttributes { timestamp, prev_randao:B256::ZERO, suggested_fee_recipient:Address::ZERO, withdrawals:Some(vec![]), parent_beacon_block_root:Some(B256::ZERO), slot_number:None, target_gas_limit:None },
            transactions:Some(transactions), no_tx_pool:Some(true), gas_limit:Some(30_000_000),
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
            ensure!(attrs.payload_attributes.timestamp == index as u64 + 1 && attrs.no_tx_pool == Some(true) && attrs.gas_limit == Some(30_000_000), "invalid frozen attributes");
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
        Ok(launch)
    }

    /// Requires actual independent first-import work; VALID/AlreadySeen is not evidence.
    pub fn check_accounting(self, before: &str, after: &str) -> Result<()> {
        if !matches!(self, Self::R | Self::D) { return Ok(()); }
        for metric in ["builder_executions", "validator_executions", "current_roots_started{source=\"builder\"}", "current_roots_started{source=\"validator\"}", "validator_ready_outputs"] {
            let key = format!("base_root_experiment_{metric}");
            ensure!(Self::counter(after, &key)? - Self::counter(before, &key)? == 1.0, "missing/excess independent first-import accounting: {key}");
        }
        let key = "base_root_experiment_trusted_insertions";
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

    /// Excludes warmup and incomplete/error samples without inventing zero observations.
    pub fn phase(samples: &[RootBenchSample], phase: &str) -> Self {
        Self::from_samples(&samples.iter().filter(|s| !s.warmup && s.error.is_none()).filter_map(|s| s.phases.get(phase).map(|i| i.end_ns - i.start_ns)).collect::<Vec<_>>())
    }
}

impl RootBenchSample {
    /// Creates a pending sample without success values.
    pub fn new(number: u64, warmup: bool) -> Self {
        Self { number, warmup, parent_hash:B256::ZERO, block_hash:None, phases:BTreeMap::new(), responses:BTreeMap::new(), pending:RootBenchPending::default(), error:None, accepted_gas:0, successful_receipts:0 }
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
        if let Some(header) = &self.pending.payload_header { self.responses.insert("payload_header".into(), serde_json::to_value(header)?); }
        Ok(())
    }

    /// Requires the exact accepted identity as well as VALID status.
    pub fn require_valid(status: &PayloadStatus, hash: B256) -> Result<()> {
        ensure!(status.status == PayloadStatusEnum::Valid && status.latest_valid_hash == Some(hash), "Engine acceptance mismatch: expected {hash}, received {status:?}"); Ok(())
    }

    /// Appends a flushed phase snapshot without overwriting earlier evidence.
    pub fn persist(&self, output: &Path) -> Result<()> {
        let mut file = OpenOptions::new().append(true).create(true).open(output.join("samples.jsonl"))?;
        serde_json::to_writer(&mut file, self)?; file.write_all(b"\n")?; file.sync_data()?; Ok(())
    }

    /// Times one bounded call without serialization or disk I/O between Engine phases.
    pub async fn call<T, E: std::fmt::Display>(&mut self, name: &str, origin: Instant, timeout: Duration, output: &Path, future: impl Future<Output = std::result::Result<T, E>>) -> Result<T> {
        let start_ns = origin.elapsed().as_nanos() as u64;
        let result = tokio::time::timeout(timeout, future).await.map_err(|e| eyre!("{name} timed out: {e}")).and_then(|result| result.map_err(|e| eyre!("{name}: {e}")));
        self.phases.insert(name.into(), RootBenchInterval { start_ns, end_ns:origin.elapsed().as_nanos() as u64 });
        if let Err(error) = &result { self.error = Some(error.to_string()); self.flush_pending()?; self.persist(output)?; }
        result
    }
}

impl RootBenchRunArgs {
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
        RootBenchBundle::numbered(&self.output)?;
        fs::create_dir(&self.output)?;
        let outcome = self.execute().await;
        if let Err(error) = &outcome {
            if !self.output.join("result.json").exists() {
                RootBenchBundle::write_new(&self.output.join("result.json"), &json!({"complete":false,"drained":false,"error":format!("{error:#}"),"cohort":self.cohort}))?;
            }
        }
        outcome
    }

    /// Runs one bounded trial and always attempts explicit runtime drain, retaining failures.
    pub async fn execute(&self) -> Result<()> {
        static RECORDER: OnceLock<std::result::Result<PrometheusHandle, String>> = OnceLock::new();
        static ACTIVE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
        let _active = ACTIVE.try_lock().map_err(|_| eyre!("only one root trial per process at a time"))?;
        ensure!(self.call_timeout_ms > 0 && self.run_timeout_ms > 0 && self.shutdown_timeout_ms > 0, "deadlines must be positive");
        RootBenchBundle::numbered(&self.output)?;
        let input_bytes = fs::read(&self.input)?;
        let bundle: RootBenchBundle = serde_json::from_slice(&input_bytes)?;
        bundle.validate()?;
        // Allocation is intentionally outside the writable node directory and is never reset.
        RootBenchBundle::write_new(&self.output.join("input.json"), &bundle)?;
        let datadir = self.output.join("datadir1");
        fs::create_dir(&datadir)?;
        let launch = self.cohort.launch_config(&bundle, &datadir)?;
        let source = std::process::Command::new("git").args(["rev-parse", "HEAD", "HEAD^{tree}"]).output()?;
        ensure!(source.status.success(), "source provenance requires a git checkout");
        let executable_digest = keccak256(fs::read(std::env::current_exe()?)?);
        let manifest: Option<Value> = self.executable_manifest.as_ref().map(|path| fs::read(path).and_then(|bytes| serde_json::from_slice(&bytes).map_err(std::io::Error::other))).transpose()?;
        RootBenchBundle::write_new(&self.output.join("executable-source.json"), &json!({"external_manifest":manifest,"verification":"external-attestation-required","executable_keccak":executable_digest}))?;
        Self::verify_manifest(manifest.as_ref(), executable_digest)?;
        RootBenchBundle::write_new(&self.output.join("executable.json"), &json!({"executable_keccak":keccak256(fs::read(std::env::current_exe()?)?),"harness_module_keccak":keccak256(include_bytes!("root_bench.rs"))}))?;
        let environment: BTreeMap<_, _> = std::env::vars().filter(|(name, _)| name.starts_with("RETH_") || name.starts_with("BASE_")).map(|(name, value)| {
            let sensitive = ["KEY", "TOKEN", "SECRET", "PASSWORD", "URL"].iter().any(|word| name.contains(word));
            (name, if sensitive { format!("keccak:{}", keccak256(value)) } else { value })
        }).collect();
        RootBenchBundle::write_new(&self.output.join("launch.json"), &json!({"cohort":self.cohort,"transport":"typed-in-memory","receipt_source":"canonical-execution-provider-receipts_by_block","config":format!("{launch:#?}"),"runtime_checkout":{"source":String::from_utf8(source.stdout)?,"cargo_lock_keccak":keccak256(fs::read("Cargo.lock")?),"reth_pin_keccak":keccak256(fs::read("etc/upstream-pins/reth.toml")?),"not_executable_provenance":true},"argv":std::env::args().collect::<Vec<_>>(),"environment":environment,"unsupported":["cpu_time","rss","io","cache_hit_miss","persistence_lag","queue_depth","paced_cadence","immutable_6268_baseline"]}))?;
        let metrics = RECORDER.get_or_init(|| PrometheusBuilder::new().install_recorder().map_err(|e| e.to_string())).as_ref().map_err(|e| eyre!("cannot own process metrics: {e}"))?.clone();
        let origin = Instant::now();
        RootBenchBundle::write_new(&self.output.join("runtime.json"), &json!({"configuration":format!("{:#?}", RuntimeConfig::default())}))?;
        let runtime = std::thread::spawn(|| RuntimeBuilder::new(RuntimeConfig::default()).build()).join().map_err(|_| eyre!("runtime construction panicked"))??;
        let mut critical = runtime.take_task_manager_handle().ok_or_else(|| eyre!("runtime critical-task monitor unavailable"))?;
        let mut launched = None;
        let mut samples = Vec::new();
        let mut measurement_start_ns = None;
        let timeout = Duration::from_millis(self.call_timeout_ms);
        let mut critical_result = None;
        let mut outcome = tokio::select! {
            result = tokio::time::timeout(Duration::from_millis(self.run_timeout_ms), async {
            launched = Some(tokio::time::timeout(timeout, launch.launch_default(CliContext { task_executor: runtime.clone() })).await.map_err(|_| eyre!("launch timed out"))??);
            let node = launched.as_ref().ok_or_else(|| eyre!("launch handle missing"))?;
            let reads = RootProvider::<Base>::new(RpcClient::new(InMemoryTransport { module:node.auth_module.clone() }, false));
            let initial = tokio::time::timeout(timeout, reads.get_block_by_number(alloy_eips::BlockNumberOrTag::Latest)).await??.ok_or_else(|| eyre!("missing genesis"))?;
            ensure!(initial.header.number == 0, "fresh-genesis benchmark cannot resume a database");
            let mut parent = initial.header.hash;
            let mut parent_root = initial.header.state_root;
            RootBenchBundle::write_raw_new(&self.output.join("metrics-before.txt"), Self::root_metrics(&metrics).as_bytes())?;
            for (index, attrs) in bundle.blocks.iter().enumerate() {
                if index as u64 == bundle.warmup { measurement_start_ns = Some(origin.elapsed().as_nanos() as u64); }
                samples.push(RootBenchSample::new(index as u64 + 1, (index as u64) < bundle.warmup));
                let sample = samples.last_mut().ok_or_else(|| eyre!("missing sample"))?;
                sample.parent_hash = parent;
                let before = Self::root_metrics(&metrics);
                RootBenchBundle::write_raw_new(&self.output.join(format!("metrics{}-before.txt", index + 1)), before.as_bytes())?;
                let result: Result<()> = async {
                    let fcu = ForkchoiceState { head_block_hash:parent, safe_block_hash:parent, finalized_block_hash:initial.header.hash };
                    let start = origin.elapsed().as_nanos() as u64;
                    sample.pending.build_fcu = Some(sample.call("build_fcu", origin, timeout, &self.output, node.engine_api.fork_choice_updated_v3(fcu, Some(attrs.clone()))).await?);
                    let started = sample.pending.build_fcu.as_ref().expect("received build response");
                    RootBenchSample::require_valid(&started.payload_status, parent)?;
                    let id = started.payload_id.ok_or_else(|| eyre!("build FCU missing payload ID"))?;
                    // AwaitInProgress resolves construction. There is deliberately no scheduling sleep.
                    let envelope = sample.call("get_payload", origin, timeout, &self.output, node.engine_api.get_payload_v4(id)).await?;
                    let available = origin.elapsed().as_nanos() as u64;
                    sample.phases.insert("payload_available".into(), RootBenchInterval { start_ns:start, end_ns:available });
                    sample.block_hash = Some(envelope.execution_payload.payload_inner.payload_inner.payload_inner.block_hash);
                    let block = RootBenchSample::payload_block(envelope.execution_payload.clone(), envelope.parent_beacon_block_root)?;
                    let hash = block.header.hash_slow();
                    sample.block_hash = Some(hash);
                    sample.pending.payload_header = Some(block.header.clone());
                    ensure!(block.header.parent_hash == parent && block.header.number == index as u64 + 1 && block.header.timestamp == attrs.payload_attributes.timestamp, "native payload identity mismatch");
                    ensure!(Some(&block.body.transactions) == attrs.transactions.as_ref(), "native payload changed signed transaction bytes/order");
                    if self.cohort == RootBenchCohort::D && attrs.payload_attributes.timestamp == bundle.activation { ensure!(block.header.state_root == parent_root, "D activation must commit the actual pre-F parent post-state"); }
                    sample.pending.new_payload = Some(sample.call("new_payload", origin, timeout, &self.output, node.engine_api.new_payload_v4(envelope.execution_payload, envelope.parent_beacon_block_root)).await?);
                    RootBenchSample::require_valid(sample.pending.new_payload.as_ref().expect("received import response"), hash)?;
                    sample.pending.canonical_fcu = Some(sample.call("canonical_fcu", origin, timeout, &self.output, node.engine_api.fork_choice_updated_v3(ForkchoiceState { head_block_hash:hash, ..fcu }, None)).await?);
                    RootBenchSample::require_valid(&sample.pending.canonical_fcu.as_ref().expect("received canonical response").payload_status, hash)?;
                    sample.phases.insert("cycle".into(), RootBenchInterval { start_ns:start, end_ns:origin.elapsed().as_nanos() as u64 });
                    sample.flush_pending()?;
                    sample.responses.insert("first_import".into(), json!(true));
                    let head = tokio::time::timeout(timeout, reads.get_block_by_number(alloy_eips::BlockNumberOrTag::Latest)).await??.ok_or_else(|| eyre!("missing accepted head"))?;
                    ensure!(head.header.hash == hash, "canonical head differs from accepted hash");
                    let transactions = attrs.transactions.as_ref().ok_or_else(|| eyre!("missing transactions"))?;
                    let provider = node.handle.node.provider.clone();
                    let receipts = tokio::time::timeout(timeout, tokio::task::spawn_blocking(move || provider.receipts_by_block(hash.into()))).await???.ok_or_else(|| eyre!("missing canonical native receipts for {hash}"))?;
                    RootBenchSample::verify_receipts(&receipts, transactions.len(), block.header.gas_used)?;
                    sample.successful_receipts = receipts.len() as u64;
                    let sender_nonce = tokio::time::timeout(timeout, reads.get_transaction_count(RootBenchBundle::freeze_identity()?)).await??;
                    let expected_nonce: usize = bundle.blocks[..=index].iter().map(|b| b.transactions.as_ref().map_or(0, Vec::len)).sum();
                    ensure!(sender_nonce == expected_nonce as u64, "accepted sender nonce mismatch");
                    let recipient_balance = tokio::time::timeout(timeout, reads.get_balance(Address::repeat_byte(2))).await??;
                    ensure!(recipient_balance == U256::from(expected_nonce), "recipient balance must reflect every accepted signed transfer exactly");
                    let after = Self::root_metrics(&metrics);
                    RootBenchBundle::write_raw_new(&self.output.join(format!("metrics{}-after.txt", index + 1)), after.as_bytes())?;
                    if self.cohort == RootBenchCohort::R || (self.cohort == RootBenchCohort::D && attrs.payload_attributes.timestamp >= bundle.activation) { self.cohort.check_accounting(&before, &after)?; }
                    sample.accepted_gas = block.header.gas_used;
                    parent = hash; parent_root = block.header.state_root;
                    Ok(())
                }.await;
                if let Err(error) = result { sample.error = Some(format!("{error:#}")); sample.flush_pending()?; sample.persist(&self.output)?; return Err(error); }
                sample.persist(&self.output)?;
            }
            Ok::<(), eyre::Report>(())
            }) => result.map_err(|_| eyre!("overall trial deadline exceeded; in-flight call outcome uncertain")).and_then(|result| result),
            failure = &mut critical => { let error = eyre!("runtime critical-task monitor ended during trial: {failure:?}"); critical_result = Some(failure); Err(error) },
            _ = tokio::signal::ctrl_c() => Err(eyre!("trial interrupted; in-flight call outcome uncertain")),
        };
        if let Err(error) = &outcome {
            if let Some(last) = samples.last_mut() {
                last.error.get_or_insert_with(|| format!("{error:#}"));
                last.flush_pending()?;
                if let Err(persist_error) = last.persist(&self.output) { outcome = Err(eyre!("cannot persist partial sample: {persist_error:#}")); }
            }
        }
        // Releasing transports/providers before graceful shutdown is part of the measurement.
        let service = launched.as_ref().and_then(|node| node.root_service.clone());
        drop(launched);
        let drain_runtime = runtime.clone();
        let shutdown = Duration::from_millis(self.shutdown_timeout_ms);
        let guards_completed = tokio::task::spawn_blocking(move || drain_runtime.graceful_shutdown_with_timeout(shutdown)).await?;
        // The task manager can exit before shutdown-task panics. Worker state is required too.
        if critical_result.is_none() {
            match tokio::time::timeout(shutdown, &mut critical).await {
                Ok(result) => critical_result = Some(result),
                Err(_) => { if outcome.is_ok() { outcome = Err(eyre!("critical-task monitor shutdown deadline exceeded")); } }
            }
        }
        let worker_result = Self::verify_worker(service.as_ref().and_then(|service| service.manager.get()), self.cohort != RootBenchCohort::O);
        let worker_joined_healthy = worker_result.is_ok();
        let drained = guards_completed && worker_joined_healthy;
        if let Err(error) = worker_result { if outcome.is_ok() { outcome = Err(error); } }
        if matches!(&critical_result, Some(Err(_)) | Some(Ok(Err(_)))) && outcome.is_ok() { outcome = Err(eyre!("critical task failure: {critical_result:?}")); }
        tokio::task::spawn_blocking(move || runtime.shutdown_timeout(shutdown)).await?;
        let end_ns = origin.elapsed().as_nanos() as u64;
        let drained_metrics = Self::root_metrics(&metrics);
        RootBenchBundle::write_raw_new(&self.output.join("metrics-drained.txt"), drained_metrics.as_bytes())?;
        if outcome.is_ok() && RootBenchCohort::counter(&drained_metrics, "base_root_experiment_jobs_failed")? != 0.0 { outcome = Err(eyre!("root worker failure observed after drain")); }
        let phases = ["build_fcu", "get_payload", "payload_available", "new_payload", "canonical_fcu", "cycle"];
        let summaries: BTreeMap<_, _> = phases.into_iter().map(|phase| (phase, RootBenchSummary::phase(&samples, phase))).collect();
        let measured: Vec<_> = samples.iter().filter(|s| !s.warmup && s.error.is_none() && s.phases.contains_key("cycle")).collect();
        let seconds = measurement_start_ns.map(|start| (end_ns - start) as f64 / 1e9);
        let complete = outcome.is_ok() && drained;
        RootBenchBundle::write_new(&self.output.join("drain.json"), &json!({"guards_completed":guards_completed,"worker_joined_healthy":worker_joined_healthy,"manager_observed":service.as_ref().is_some_and(|s| s.manager.get().is_some()),"critical_task_result":format!("{critical_result:?}")}))?;
        RootBenchBundle::write_new(&self.output.join("result.json"), &json!({"complete":complete,"error":outcome.as_ref().err().map(|e| format!("{e:#}")),"drained":drained,"cohort":self.cohort,"input_digest":bundle.digest,"samples":samples,"summaries":summaries,"measurement_start_ns":measurement_start_ns,"drain_end_ns":end_ns,"accepted_gas_per_second":seconds.map(|s| measured.iter().map(|row| row.accepted_gas).sum::<u64>() as f64 / s),"successful_receipts_per_second":seconds.map(|s| measured.iter().map(|row| row.successful_receipts).sum::<u64>() as f64 / s)}))?;
        outcome?; ensure!(drained, "shutdown/drain deadline exceeded; trial incomplete"); Ok(())
    }

    /// Preserves process-level experiment series verbatim in the exporter's representation.
    pub fn root_metrics(handle: &PrometheusHandle) -> String {
        handle.render().lines().filter(|line| line.starts_with("base_root_experiment_") || line.contains(" base_root_experiment_")).map(|line| format!("{line}\n")).collect()
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
            let failed = if timeout { sample.call(failed_phase, Instant::now(), Duration::from_millis(1), directory.path(), std::future::pending::<std::result::Result<(), &str>>()).await } else { sample.call(failed_phase, Instant::now(), Duration::from_secs(1), directory.path(), std::future::ready(Err::<(), _>("injected transport failure"))).await };
            assert!(failed.is_err(), "later transport failure or timeout must propagate");
            let saved: Value = serde_json::from_str(fs::read_to_string(directory.path().join("samples.jsonl"))?.trim())?;
            assert_eq!(saved["responses"]["build_fcu"]["payloadId"], "0x0101010101010101", "build payload ID must survive every later failure");
            if failed_phase == "canonical_fcu" { assert_eq!(saved["responses"]["new_payload"]["status"], "VALID", "preceding import response must survive canonical failure"); }
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
        rows[1].phases.insert("cycle".into(), RootBenchInterval { start_ns:2, end_ns:12 });
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
        assert!(RootBenchCohort::R.check_accounting("", "").is_err(), "AlreadySeen VALID without actual independent execution must reject");
        let metrics = "base_root_experiment_builder_executions 1\nbase_root_experiment_validator_executions 1\nbase_root_experiment_current_roots_started{source=\"builder\"} 1\nbase_root_experiment_current_roots_started{source=\"validator\"} 1\nbase_root_experiment_validator_ready_outputs 1\n";
        RootBenchCohort::R.check_accounting("", metrics)?;
        assert!(RootBenchCohort::D.check_accounting("", &format!("{metrics}base_root_experiment_trusted_insertions 1\n")).is_err(), "trusted insertion must invalidate R/D evidence");
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
        let failed = sample.call("new_payload", Instant::now(), Duration::from_millis(1), directory.path(), std::future::pending::<std::result::Result<(), std::io::Error>>()).await;
        assert!(failed.is_err(), "a never-completing Engine call must hit its deadline without retry");
        assert!(sample.error.as_ref().unwrap().contains("timed out"), "timeout must remain an explicit failure");
        assert_eq!(RootBenchSummary::phase(&[sample], "new_payload").count, 0, "timeout must not count as a successful latency");
        assert_eq!(fs::read_to_string(directory.path().join("samples.jsonl"))?.lines().count(), 1, "exactly one failed phase record must survive");
        let input = directory.path().join("bad1.json");
        RootBenchBundle::write_new(&input, &json!({}))?;
        let output = directory.path().join("trial1");
        let args = RootBenchRunArgs { input, output:output.clone(), executable_manifest:None, cohort:RootBenchCohort::R, call_timeout_ms:1, run_timeout_ms:10, shutdown_timeout_ms:10 };
        assert!(args.run().await.is_err(), "invalid frozen input must fail before launch");
        let original = fs::read(output.join("result.json"))?;
        let report: Value = serde_json::from_slice(&original)?;
        assert_eq!(report["complete"], false, "pre-launch failure cannot claim completion");
        assert!(!output.join("datadir1").exists(), "invalid input must never create writable node state");
        assert!(fs::create_dir(&output).is_err(), "a failed trial's numbered output cannot be reused");
        assert_eq!(fs::read(output.join("result.json"))?, original, "refused reuse must preserve failure bytes");
        Ok(())
    }

    /// An otherwise valid workload cannot launch under runtime-checkout-only provenance.
    #[tokio::test]
    async fn unverified_executable_refuses_launch() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let input = directory.path().join("bundle1.json");
        RootBenchBundle::write_new(&input, &RootBenchBundle::freeze(4, 1, 1)?)?;
        let output = directory.path().join("trial1");
        let args = RootBenchRunArgs { input, output:output.clone(), executable_manifest:None, cohort:RootBenchCohort::R, call_timeout_ms:1, run_timeout_ms:10, shutdown_timeout_ms:10 };
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
            let executable_manifest = Some(PathBuf::from(std::env::var("ROOT_BENCH_EXECUTABLE_MANIFEST").map_err(|_| eyre!("supported-host integration requires externally supplied manifest for this test executable"))?));
            let args = RootBenchRunArgs { input, output: dir.path().join("trial1"), executable_manifest, cohort, call_timeout_ms: 30_000, run_timeout_ms: 180_000, shutdown_timeout_ms: 30_000 };
            args.run().await?;
            let result: serde_json::Value = serde_json::from_slice(&std::fs::read(dir.path().join("trial1/result.json"))?)?;
            assert_eq!(result["complete"], true, "actual admitted R/D trial must drain and succeed");
            assert_eq!(result["samples"].as_array().unwrap().len(), 4, "all activation boundary blocks must be recorded");
            assert_eq!(result["summaries"]["cycle"]["count"], 3, "one warmup must be excluded");
            for sample in result["samples"].as_array().unwrap() {
                assert_eq!(sample["successful_receipts"], 2, "both native receipts must be independently checked at each boundary block");
                assert_eq!(sample["accepted_gas"], 42_000, "both transfers must contribute exact canonical gas");
            }
            let launch: Value = serde_json::from_slice(&fs::read(dir.path().join("trial1/launch.json"))?)?;
            assert_eq!(launch["receipt_source"], "canonical-execution-provider-receipts_by_block", "EL-only fixture must not depend on L1-info RPC enrichment");
            let drain: Value = serde_json::from_slice(&fs::read(dir.path().join("trial1/drain.json"))?)?;
            assert_eq!(drain["manager_observed"], true, "active R/D trials must observe the actual shared manager");
            assert_eq!(drain["worker_joined_healthy"], true, "guard completion alone must not establish successful drain");
        }
        Ok(())
    }
}
