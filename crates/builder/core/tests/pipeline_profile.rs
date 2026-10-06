//! Repeatable production-pipeline benchmark using independent MDBX-backed builder and validator nodes.
//!
//! Run alone in a release build with `--ignored --nocapture --test-threads=1`.
//! Consecutive Prometheus sum/count deltas retain exact per-block timings without depending on
//! histogram buckets. Node metrics and all required production behavior remain enabled.

use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

use alloy_consensus::TxEip1559;
use alloy_eips::Encodable2718;
use alloy_primitives::{Address, Bytes, TxKind, U256, hex};
use alloy_provider::Provider;
use base_builder_core::{
    BuilderConfig, BuilderMetrics,
    test_utils::{
        builder_signer, default_node_config, default_node_config_with_azul,
        generate_signer_from_seed, setup_test_instance_with_node_config, sign_base_tx,
    },
};
use base_common_consensus::{BaseTypedTransaction, TxDeposit};
use metrics_exporter_prometheus::PrometheusHandle;
use parking_lot::Mutex;
use serde::Serialize;

/// Declared normal/stress load shapes supplement the original reference metric.
#[derive(Debug)]
pub struct PipelineScenario {
    /// Recorded scenario class.
    pub name: String,
    /// User transactions per block.
    pub transactions: usize,
    /// Fresh slots per storage call.
    pub storage_slots: usize,
    /// Builder block cadence.
    pub block_time: Duration,
    /// Block gas budget.
    pub gas_limit: u64,
    /// Spread submissions over the build window.
    pub streamed: bool,
}

impl PipelineScenario {
    /// Maximum sparse MDBX map size for benchmark state growth and persisted trie updates.
    pub const DATABASE_MAX_SIZE_BYTES: usize = 64 * 1024 * 1024;

    /// Resolve the frozen load-shape definitions.
    pub fn from_environment(storage: bool) -> eyre::Result<Self> {
        let name = std::env::var("PIPELINE_LOAD_CLASS").unwrap_or_else(|_| "reference".to_owned());
        let (transactions, storage_slots, block_time, gas_limit, streamed) = match name.as_str() {
            "reference" => (1024, 1, Duration::from_secs(1), 100_000_000, false),
            "normal" => {
                (160, if storage { 8 } else { 1 }, Duration::from_secs(2), 400_000_000, true)
            }
            "stress" => {
                (if storage { 2048 } else { 2400 }, 1, Duration::from_secs(1), 100_000_000, false)
            }
            _ => eyre::bail!("unknown load class"),
        };
        Ok(Self { name, transactions, storage_slots, block_time, gas_limit, streamed })
    }
}

/// Direct production deadline/frame observations; all misses must be zero.
#[derive(Debug, Serialize)]
pub struct DeadlineObservation {
    /// Payload-job expiration counter delta, including verification.
    pub payload_job_expirations: u64,
    /// Missing-flashblocks histogram sum delta.
    pub missing_flashblocks: f64,
    /// Late-attributes reduction histogram sum delta.
    pub reduced_flashblocks: f64,
    /// Conservative block-time-plus-leeway budget, in seconds.
    pub build_wall_budget_seconds: f64,
    /// Whether that budget was exceeded.
    pub build_wall_budget_missed: bool,
}

/// Snapshots production histogram sums and counts after each completed block.
#[derive(Debug)]
pub struct PipelineRecorder {
    /// Existing node-owned recorder.
    pub handle: PrometheusHandle,
    /// Last observed cumulative values.
    pub previous: Mutex<BTreeMap<String, f64>>,
}

impl Default for PipelineRecorder {
    fn default() -> Self {
        Self::new()
    }
}

impl PipelineRecorder {
    /// Use the node recorder instead of installing a competing global recorder.
    pub fn new() -> Self {
        Self {
            handle: reth_node_metrics::recorder::install_prometheus_recorder().handle().clone(),
            previous: Mutex::new(BTreeMap::new()),
        }
    }

    /// Return exact timing deltas; a successful build must contribute one count.
    pub fn drain(&self) -> (BTreeMap<String, Vec<f64>>, BTreeMap<String, u64>) {
        let current: BTreeMap<String, f64> = self
            .handle
            .render()
            .lines()
            .filter_map(|line| {
                let (name, value) = line.split_once(' ')?;
                let name = name.strip_prefix("reth_")?;
                if name.starts_with("base_builder_")
                    && !name.contains('{')
                    && (name.ends_with("duration_sum") || name.ends_with("duration_count"))
                {
                    Some((name.to_owned(), value.parse().ok()?))
                } else {
                    None
                }
            })
            .collect();
        let mut previous = self.previous.lock();
        let mut counts = BTreeMap::new();
        let observations = current
            .iter()
            .filter_map(|(name, value)| {
                let metric = name.strip_suffix("_sum")?;
                let count_name = format!("{metric}_count");
                let count = current.get(&count_name).copied().unwrap_or_default()
                    - previous.get(&count_name).copied().unwrap_or_default();
                let delta = value - previous.get(name).copied().unwrap_or_default();
                counts.insert(metric.to_owned(), count as u64);
                let values = if count > 0.0 { vec![delta] } else { vec![] };
                Some((metric.to_owned(), values))
            })
            .collect();
        *previous = current;
        (observations, counts)
    }

    /// Read direct safety metrics, rejecting missing post-build observations.
    pub fn deadline_totals(&self, after_build: bool) -> eyre::Result<(u64, f64, f64)> {
        let rendered = self.handle.render();
        let read = |prefix: &str, required: bool| -> eyre::Result<f64> {
            let value = rendered
                .lines()
                .filter_map(|line| line.split_once(' '))
                .find(|(name, _)| name.starts_with(prefix) && !name.contains('{'))
                .map(|(_, value)| value.parse::<f64>())
                .transpose()?;
            if required {
                value.ok_or_else(|| eyre::eyre!("missing deadline metric: {prefix}"))
            } else {
                Ok(value.unwrap_or_default())
            }
        };
        Ok((
            read("reth_base_builder_payload_job_deadline_misses", true)? as u64,
            read("reth_base_builder_missing_flashblocks_count_sum", after_build)?,
            read("reth_base_builder_reduced_flashblocks_number_sum", after_build)?,
        ))
    }
}

/// One validated canonical block and its raw pipeline timings in seconds.
#[derive(Debug, Serialize)]
pub struct BlockSample {
    /// Workload-local block index.
    pub index: usize,
    /// Canonical block hash, checked against a separately re-executing node.
    pub hash: String,
    /// Number of transactions, including the L1 attributes deposit.
    pub transactions: usize,
    /// Consensus gas used.
    pub gas_used: u64,
    /// Canonical Jovian DA footprint gas, sharing the block resource budget.
    pub da_footprint_gas: u64,
    /// Driver request duration, including its configured slot wait and RPC/import.
    pub driver_wall_seconds: f64,
    /// Per-block duration sums; active and wall each contain exactly one observation.
    pub timings: BTreeMap<String, Vec<f64>>,
    /// Actual histogram observation counts (stage samples are not reconstructed).
    pub observations: BTreeMap<String, u64>,
    /// Fallback plus all five scheduled flashblocks must be published.
    pub published_flashblocks: usize,
    /// Recorded reference, normal or stress class.
    pub load_class: String,
    /// Declared user count plus the attributes deposit.
    pub expected_transactions: usize,
    /// Explicit deadline safety evidence.
    pub deadlines: DeadlineObservation,
}

/// Profile one frozen workload; configure the workload name and output path through environment.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "release-only performance experiment; runs two real execution nodes"]
pub async fn profile_pipeline() -> eyre::Result<()> {
    let workload = std::env::var("PIPELINE_WORKLOAD")?;
    let output = std::env::var("PIPELINE_OUTPUT")?;
    let azul = workload.ends_with("azul");
    let storage = workload.starts_with("storage");
    let scenario = PipelineScenario::from_environment(storage)?;
    let builder_config =
        BuilderConfig::for_tests().with_block_time_ms(scenario.block_time.as_millis() as u64);
    let wall_budget = builder_config.block_time + builder_config.block_time_leeway;
    eyre::ensure!(
        ["transfer-legacy", "transfer-azul", "storage-legacy", "storage-azul"]
            .contains(&workload.as_str()),
        "unknown workload"
    );
    let mut node_config =
        if azul { default_node_config_with_azul() } else { default_node_config() };
    node_config.txpool.max_account_slots = if scenario.name == "stress" { 8192 } else { 2048 };
    node_config.db.max_size = Some(PipelineScenario::DATABASE_MAX_SIZE_BYTES);
    let instance = setup_test_instance_with_node_config(builder_config, node_config).await?;
    let mut validator_config =
        if azul { default_node_config_with_azul() } else { default_node_config() };
    validator_config.db.max_size = Some(PipelineScenario::DATABASE_MAX_SIZE_BYTES);
    let validator =
        setup_test_instance_with_node_config(BuilderConfig::for_tests(), validator_config).await?;
    let recorder = PipelineRecorder::new();
    BuilderMetrics::payload_job_deadline_misses().increment(0);
    let driver = instance.driver().await?.with_gas_limit(scenario.gas_limit);
    let validator_driver = validator.driver().await?.with_gas_limit(scenario.gas_limit);
    let signers = if scenario.streamed {
        (0..16)
            .map(|index| generate_signer_from_seed(&format!("pipeline-normal-{index}")))
            .collect::<Vec<_>>()
    } else {
        vec![builder_signer()]
    };
    let mut nonces = vec![0; signers.len()];
    if scenario.streamed {
        let funding = signers
            .iter()
            .map(|signer| {
                let deposit = TxDeposit {
                    from: signer.address(),
                    to: TxKind::Call(signer.address()),
                    mint: 100_000_000_000_000_000_000_000,
                    gas_limit: 21_000,
                    ..Default::default()
                };
                Ok::<Bytes, eyre::Report>(
                    sign_base_tx(signer, BaseTypedTransaction::Deposit(deposit))?
                        .encoded_2718()
                        .into(),
                )
            })
            .collect::<eyre::Result<Vec<_>>>()?;
        let block = driver.build_new_block_with_txs(funding.clone()).await?;
        let checked = validator_driver
            .build_new_block_with_txs_timestamp(
                funding,
                Some(true),
                Some(Duration::from_secs(block.header.timestamp)),
                None,
                Some(0),
            )
            .await?;
        eyre::ensure!(checked.header.hash == block.header.hash, "funding parity failure");
        nonces.fill(1);
    }
    let mut contract = Address::ZERO;
    if storage {
        // Runtime: write calldata word 1 to the slot in word 0; no Solidity toolchain required.
        let deployment_input: Bytes = if scenario.storage_slots == 1 {
            hex!("600a600c600039600a6000f360203560003555600000").into()
        } else {
            let mut runtime = Vec::new();
            for slot in 0..scenario.storage_slots {
                runtime.extend_from_slice(&hex!("602035600035"));
                runtime.extend_from_slice(&[0x60, slot as u8, 0x01, 0x55]);
            }
            runtime.push(0x00);
            let length = runtime.len() as u8;
            let mut init = vec![0x60, length, 0x60, 12, 0x60, 0, 0x39, 0x60, length, 0x60, 0, 0xf3];
            init.extend(runtime);
            init.into()
        };
        let deployment = driver
            .create_transaction()
            .with_signer(&signers[0])
            .with_nonce(nonces[0])
            .with_create()
            .with_input(deployment_input)
            .with_gas_limit(100_000)
            .with_max_fee_per_gas(1_000_000_000)
            .build()
            .await;
        contract = signers[0].address().create(nonces[0]);
        let raw: Bytes = deployment.encoded_2718().into();
        let block = driver.build_new_block_with_txs(vec![raw.clone()]).await?;
        let checked = validator_driver
            .build_new_block_with_txs_timestamp(
                vec![raw],
                Some(true),
                Some(Duration::from_secs(block.header.timestamp)),
                None,
                Some(0),
            )
            .await?;
        eyre::ensure!(checked.header.hash == block.header.hash, "deployment parity failure");
        nonces[0] += 1;
    }
    let listener = instance.spawn_flashblocks_listener();
    tokio::time::sleep(Duration::from_millis(50)).await;
    recorder.drain();
    let mut results = Vec::new();
    // Identical warm-up and measurement counts for all declared load shapes.
    for index in 0..12 {
        let mut raw = Vec::with_capacity(scenario.transactions);
        let mut expected = Vec::with_capacity(scenario.transactions);
        for transaction in 0..scenario.transactions {
            let sender_index = transaction % signers.len();
            let recipient = Address::from_word(
                U256::from(10_000 + index * scenario.transactions + transaction).into(),
            );
            let gas_limit = if storage {
                if scenario.storage_slots == 1 { 60_000 } else { 250_000 }
            } else {
                21_000
            };
            let input: Bytes = if storage {
                let mut input = U256::from(
                    (index * scenario.transactions + transaction) * scenario.storage_slots,
                )
                .to_be_bytes::<32>()
                .to_vec();
                input.extend_from_slice(&U256::from(1).to_be_bytes::<32>());
                input.into()
            } else {
                Bytes::new()
            };
            let tx = sign_base_tx(
                &signers[sender_index],
                BaseTypedTransaction::Eip1559(TxEip1559 {
                    chain_id: 901,
                    nonce: nonces[sender_index],
                    gas_limit,
                    to: TxKind::Call(if storage { contract } else { recipient }),
                    value: U256::from(if storage { 0 } else { 1 }),
                    input,
                    max_fee_per_gas: 1_000_000_000,
                    max_priority_fee_per_gas: 1,
                    ..Default::default()
                }),
            )?;
            expected.push(tx.tx_hash());
            let bytes: Bytes = tx.encoded_2718().into();
            if !scenario.streamed {
                let _submission = driver.provider().send_raw_transaction(&bytes).await?;
            }
            raw.push(bytes);
            nonces[sender_index] += 1;
        }
        recorder.drain();
        let deadline_before = recorder.deadline_totals(false)?;
        let start = Instant::now();
        let block = if scenario.streamed {
            let producer = async {
                tokio::time::sleep(Duration::from_millis(50)).await;
                for batch in raw.chunks(scenario.transactions / 10) {
                    for bytes in batch {
                        let _pending = driver.provider().send_raw_transaction(bytes).await?;
                    }
                    tokio::time::sleep(Duration::from_millis(150)).await;
                }
                Ok::<(), eyre::Report>(())
            };
            let (submitted, built) = tokio::join!(
                producer,
                tokio::time::timeout(Duration::from_secs(15), driver.build_new_block())
            );
            submitted?;
            built??
        } else {
            tokio::time::timeout(Duration::from_secs(15), driver.build_new_block()).await??
        };
        let driver_wall_seconds = start.elapsed().as_secs_f64();
        let (timings, observations) = recorder.drain();
        let deadline_after = recorder.deadline_totals(true)?;
        let expected_publications = scenario.block_time.as_millis() as usize / 200 + 1;
        let published_flashblocks = listener
            .flashblocks
            .lock()
            .iter()
            .filter(|fb| {
                fb.metadata.get("block_number").and_then(serde_json::Value::as_u64)
                    == Some(block.header.number)
            })
            .count();
        eyre::ensure!(
            published_flashblocks == expected_publications,
            "missing flashblocks: {published_flashblocks}"
        );
        eyre::ensure!(
            block.transactions.len() == scenario.transactions + 1,
            "incomplete inclusion: {}",
            block.transactions.len()
        );
        eyre::ensure!(
            expected
                .iter()
                .all(|hash| block.transactions.hashes().any(|included| included == *hash)),
            "transaction inclusion mismatch"
        );
        // Preserve canonical producer ordering across independent nonce lanes.
        let raw_by_hash: BTreeMap<_, _> = expected.iter().copied().zip(raw).collect();
        let raw =
            block.transactions.hashes().skip(1).map(|hash| raw_by_hash[&hash].clone()).collect();
        let checked = validator_driver
            .build_new_block_with_txs_timestamp(
                raw,
                Some(true),
                Some(Duration::from_secs(block.header.timestamp)),
                None,
                Some(0),
            )
            .await?;
        eyre::ensure!(
            checked.header.hash == block.header.hash,
            "canonical builder/validator parity failure"
        );
        let deadlines = DeadlineObservation {
            payload_job_expirations: recorder.deadline_totals(true)?.0 - deadline_before.0,
            missing_flashblocks: deadline_after.1 - deadline_before.1,
            reduced_flashblocks: deadline_after.2 - deadline_before.2,
            build_wall_budget_seconds: wall_budget.as_secs_f64(),
            build_wall_budget_missed: timings["base_builder_block_build_wall_duration"][0]
                > wall_budget.as_secs_f64(),
        };
        eyre::ensure!(
            deadlines.payload_job_expirations == 0
                && deadlines.missing_flashblocks == 0.0
                && deadlines.reduced_flashblocks == 0.0
                && !deadlines.build_wall_budget_missed,
            "deadline guardrail failed: {deadlines:?}"
        );
        if index >= 2 {
            eyre::ensure!(
                observations.get("base_builder_active_block_build_duration") == Some(&1),
                "expected one successful builder observation"
            );
            eyre::ensure!(
                timings
                    .get("base_builder_active_block_build_duration")
                    .is_some_and(|v| v.len() == 1),
                "missing or duplicate active pipeline observation: {timings:?}"
            );
            results.push(BlockSample {
                index,
                hash: block.header.hash.to_string(),
                transactions: block.transactions.len(),
                gas_used: block.header.gas_used,
                da_footprint_gas: block.header.blob_gas_used.unwrap_or_default(),
                driver_wall_seconds,
                timings,
                observations,
                published_flashblocks,
                load_class: scenario.name.clone(),
                expected_transactions: scenario.transactions + 1,
                deadlines,
            });
        }
    }
    listener.stop().await?;
    std::fs::write(output, serde_json::to_vec_pretty(&results)?)?;
    drop(driver);
    drop(validator_driver);
    drop(instance);
    drop(validator);
    Ok(())
}
