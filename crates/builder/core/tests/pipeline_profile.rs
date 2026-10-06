//! Repeatable production-pipeline benchmark using independent MDBX-backed builder and validator nodes.
//!
//! Run alone in a release build with `--ignored --nocapture --test-threads=1`.
//! Consecutive Prometheus sum/count deltas retain exact per-block timings without depending on
//! histogram buckets. Node metrics and all required production behavior remain enabled.

use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

use alloy_eips::Encodable2718;
use alloy_primitives::{Address, Bytes, U256, hex};
use alloy_provider::Provider;
use base_builder_core::{
    BuilderConfig,
    test_utils::{
        builder_signer, default_node_config, default_node_config_with_azul,
        setup_test_instance_with_node_config,
    },
};
use metrics_exporter_prometheus::PrometheusHandle;
use parking_lot::Mutex;
use serde::Serialize;

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
    /// Driver request duration, including its configured slot wait and RPC/import.
    pub driver_wall_seconds: f64,
    /// Per-block duration sums; active and wall each contain exactly one observation.
    pub timings: BTreeMap<String, Vec<f64>>,
    /// Actual histogram observation counts (stage samples are not reconstructed).
    pub observations: BTreeMap<String, u64>,
    /// Fallback plus all five scheduled flashblocks must be published.
    pub published_flashblocks: usize,
}

/// Profile one frozen workload; configure the workload name and output path through environment.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "release-only performance experiment; runs two real execution nodes"]
pub async fn profile_pipeline() -> eyre::Result<()> {
    let workload = std::env::var("PIPELINE_WORKLOAD")?;
    let output = std::env::var("PIPELINE_OUTPUT")?;
    let azul = workload.ends_with("azul");
    let storage = workload.starts_with("storage");
    eyre::ensure!(
        ["transfer-legacy", "transfer-azul", "storage-legacy", "storage-azul"]
            .contains(&workload.as_str()),
        "unknown workload"
    );
    let mut node_config =
        if azul { default_node_config_with_azul() } else { default_node_config() };
    node_config.txpool.max_account_slots = 2048;
    let instance =
        setup_test_instance_with_node_config(BuilderConfig::for_tests(), node_config).await?;
    let validator_config =
        if azul { default_node_config_with_azul() } else { default_node_config() };
    let validator =
        setup_test_instance_with_node_config(BuilderConfig::for_tests(), validator_config).await?;
    let recorder = PipelineRecorder::new();
    let driver = instance.driver().await?.with_gas_limit(100_000_000);
    let validator_driver = validator.driver().await?.with_gas_limit(100_000_000);
    let signer = builder_signer();
    let mut nonce = 0;
    let mut contract = Address::ZERO;
    if storage {
        // Runtime: write calldata word 1 to the slot in word 0; no Solidity toolchain required.
        let deployment = driver
            .create_transaction()
            .with_signer(&signer)
            .with_nonce(nonce)
            .with_create()
            .with_input(hex!("600a600c600039600a6000f360203560003555600000").into())
            .with_gas_limit(100_000)
            .with_max_fee_per_gas(1_000_000_000)
            .build()
            .await;
        contract = signer.address().create(nonce);
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
        nonce += 1;
    }
    let listener = instance.spawn_flashblocks_listener();
    tokio::time::sleep(Duration::from_millis(50)).await;
    recorder.drain();
    let mut results = Vec::new();
    // Fixed two-block warm-up followed by ten measured blocks, each with 1,024 transactions.
    for index in 0..12 {
        let mut raw = Vec::with_capacity(1024);
        let mut expected = Vec::with_capacity(1024);
        for transaction in 0..1024 {
            let recipient =
                Address::from_word(U256::from(10_000 + index * 1024 + transaction).into());
            let builder = driver
                .create_transaction()
                .with_signer(&signer)
                .with_nonce(nonce)
                .with_to(if storage { contract } else { recipient })
                .with_value(if storage { 0 } else { 1 })
                .with_gas_limit(if storage { 60_000 } else { 21_000 })
                .with_max_fee_per_gas(1_000_000_000)
                .with_max_priority_fee_per_gas(1);
            let builder = if storage {
                let mut input = U256::from(index * 1024 + transaction).to_be_bytes::<32>().to_vec();
                input.extend_from_slice(&U256::from(1).to_be_bytes::<32>());
                builder.with_input(input.into())
            } else {
                builder
            };
            let tx = builder.build().await;
            expected.push(tx.tx_hash());
            let bytes: Bytes = tx.encoded_2718().into();
            let _pending = driver.provider().send_raw_transaction(&bytes).await?;
            raw.push(bytes);
            nonce += 1;
        }
        recorder.drain();
        let start = Instant::now();
        let block =
            tokio::time::timeout(Duration::from_secs(15), driver.build_new_block()).await??;
        let driver_wall_seconds = start.elapsed().as_secs_f64();
        let (timings, observations) = recorder.drain();
        let published_flashblocks = listener
            .flashblocks
            .lock()
            .iter()
            .filter(|fb| {
                fb.metadata.get("block_number").and_then(serde_json::Value::as_u64)
                    == Some(block.header.number)
            })
            .count();
        eyre::ensure!(published_flashblocks == 6, "missing flashblocks: {published_flashblocks}");
        eyre::ensure!(
            block.transactions.len() == 1025,
            "incomplete inclusion: {}",
            block.transactions.len()
        );
        eyre::ensure!(
            expected
                .iter()
                .all(|hash| block.transactions.hashes().any(|included| included == *hash)),
            "transaction inclusion mismatch"
        );
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
                driver_wall_seconds,
                timings,
                observations,
                published_flashblocks,
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
