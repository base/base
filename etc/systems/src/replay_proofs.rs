//! Proofs mode of `base-bench replay`: while blocks are inserted into a proofs-enabled node,
//! records how far proof history trails the head and times the proof host's L2 calls.

use std::{
    collections::BTreeMap,
    fmt::Display,
    future::IntoFuture,
    num::NonZeroUsize,
    sync::{Arc, Mutex},
    thread::JoinHandle,
    time::{Duration, Instant},
};

use alloy_eips::eip2718::Encodable2718;
use alloy_network::Network;
use alloy_primitives::B64;
use alloy_provider::{Provider, RootProvider};
use alloy_rpc_types_eth::BlockNumberOrTag;
use base_common_consensus::Predeploys;
use base_common_network::Base;
use base_common_rpc_types_engine::BasePayloadAttributes;
use eyre::{Result, WrapErr};
use serde::Serialize;
use serde_json::value::RawValue;
use tokio::{
    sync::{Semaphore, watch},
    task::JoinSet,
};
use tracing::warn;
use url::Url;

/// Bound on each probe call, so a node that stops answering cannot hold back the result.
const PROBE_CALL_TIMEOUT: Duration = Duration::from_secs(60);

use crate::LatencySummary;

/// Nearest-rank percentiles of integer samples.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CountSummary {
    /// Number of samples.
    pub samples: u64,
    /// Nearest-rank median.
    pub p50: u64,
    /// Nearest-rank 99th percentile.
    pub p99: u64,
    /// Maximum.
    pub max: u64,
}

impl CountSummary {
    /// Summarizes samples; all fields are zero without samples.
    pub fn from_values(values: &[u64]) -> Self {
        let mut sorted = values.to_vec();
        sorted.sort_unstable();
        let rank = |percentile: usize| {
            let index = (percentile * sorted.len()).div_ceil(100).max(1) - 1;
            sorted.get(index).copied().unwrap_or_default()
        };
        Self {
            samples: sorted.len() as u64,
            p50: rank(50),
            p99: rank(99),
            max: sorted.last().copied().unwrap_or_default(),
        }
    }
}

/// Successful-call latency and error count of one RPC method.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProofsMethodSummary {
    /// Failed calls, excluded from `latency`.
    pub errors: u64,
    /// Latency of successful calls.
    pub latency: LatencySummary,
}

/// The `proofs` section of `replay-result.json`. Recorded only; it never fails the run.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProofsSummary {
    /// Inserted head minus `debug_proofsSyncStatus.latest`, sampled when the head moves; heads
    /// inserted during a status call are not sampled.
    pub head_lag_blocks: CountSummary,
    /// Status calls that failed, timed out or returned no `latest` block, and so gave no sample.
    pub failed_probes: u64,
    /// Per JSON-RPC method name.
    pub methods: BTreeMap<String, ProofsMethodSummary>,
    /// JSON size of each `debug_executePayload` witness, in bytes.
    pub witness_bytes: CountSummary,
}

/// Raw samples behind a [`ProofsSummary`].
#[derive(Debug, Default)]
pub struct ProofsSamples {
    /// Inserted head minus proof-history head.
    pub head_lag_blocks: Vec<u64>,
    /// Status calls that gave no lag sample.
    pub failed_probes: u64,
    /// Successful-call latencies in milliseconds and the error count, per method.
    pub methods: BTreeMap<&'static str, (Vec<f64>, u64)>,
    /// Witness sizes in bytes.
    pub witness_bytes: Vec<u64>,
}

impl ProofsSamples {
    /// Reduces the samples to percentiles.
    pub fn summarize(&self) -> ProofsSummary {
        let methods = self.methods.iter().map(|(method, (latencies, errors))| {
            let latency = LatencySummary::from_millis(latencies);
            (method.to_string(), ProofsMethodSummary { errors: *errors, latency })
        });
        ProofsSummary {
            head_lag_blocks: CountSummary::from_values(&self.head_lag_blocks),
            failed_probes: self.failed_probes,
            methods: methods.collect(),
            witness_bytes: CountSummary::from_values(&self.witness_bytes),
        }
    }
}

/// Prober on its own thread and runtime, so its calls stay off the runtime that serves the
/// engine API.
#[derive(Debug)]
pub struct ReplayProofsProbe {
    /// Latest inserted block number, published by the replay loop.
    pub head: watch::Sender<u64>,
    thread: JoinHandle<Result<ProofsSummary>>,
}

impl ReplayProofsProbe {
    /// Starts probing the node at `rpc_url` each time `head` moves.
    pub fn start(rpc_url: Url, concurrency: NonZeroUsize, boundary: u64) -> Self {
        let (head, head_rx) = watch::channel(boundary);
        let provider = RootProvider::<Base>::new_http(rpc_url);
        let thread = std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .wrap_err("failed to build the proofs probe runtime")?
                .block_on(Self::run(provider, head_rx, concurrency.get()))
        });
        Self { head, thread }
    }

    /// Stops probing once in-flight calls finish and summarizes the samples.
    pub async fn finish(self) -> Result<ProofsSummary> {
        drop(self.head);
        let thread = self.thread;
        tokio::task::spawn_blocking(move || thread.join())
            .await?
            .map_err(|_| eyre::eyre!("proofs probe thread panicked"))?
    }

    async fn run(
        provider: RootProvider<Base>,
        mut head_rx: watch::Receiver<u64>,
        concurrency: usize,
    ) -> Result<ProofsSummary> {
        let samples = Arc::new(Mutex::new(ProofsSamples::default()));
        let permits = Arc::new(Semaphore::new(concurrency));
        let mut probes = JoinSet::new();
        let mut last_probed = 0;
        while head_rx.changed().await.is_ok() {
            let head = *head_rx.borrow_and_update();
            let status =
                provider.raw_request::<_, serde_json::Value>("debug_proofsSyncStatus".into(), ());
            let status = Self::timed(&samples, "debug_proofsSyncStatus", status).await;
            let Some(latest) = status.and_then(|status| status["latest"].as_u64()) else {
                samples.lock().unwrap().failed_probes += 1;
                continue;
            };
            samples.lock().unwrap().head_lag_blocks.push(head.saturating_sub(latest));
            // Probe the newest block proof history covers; while every permit is busy, skip
            // blocks so a slow node is sampled rather than queued.
            if latest > last_probed
                && let Ok(permit) = Arc::clone(&permits).try_acquire_owned()
            {
                last_probed = latest;
                let (provider, samples) = (provider.clone(), Arc::clone(&samples));
                probes.spawn(async move {
                    Self::probe(&provider, &samples, latest).await;
                    drop(permit);
                });
            }
            while probes.try_join_next().is_some() {}
        }
        probes.join_all().await;
        Ok(samples.lock().unwrap().summarize())
    }

    /// The proof host's per-block calls: the block, its payload witness, and the
    /// `L2ToL1MessagePasser` proof that output roots commit to.
    async fn probe(provider: &RootProvider<Base>, samples: &Mutex<ProofsSamples>, number: u64) {
        let block = provider.get_block_by_number(BlockNumberOrTag::Number(number)).full();
        let Some(Some(block)) = Self::timed(samples, "eth_getBlockByNumber", block).await else {
            return;
        };
        let (parent_hash, hash) = (block.header.parent_hash, block.header.hash);
        let witness = provider.client().request::<_, Box<RawValue>>(
            "debug_executePayload",
            (parent_hash, Self::payload_attributes(block)),
        );
        if let Some(witness) = Self::timed(samples, "debug_executePayload", witness).await {
            samples.lock().unwrap().witness_bytes.push(witness.get().len() as u64);
        }
        let proof = provider
            .get_proof(Predeploys::L2_TO_L1_MESSAGE_PASSER, Vec::new())
            .block_id(hash.into());
        Self::timed(samples, "eth_getProof", proof).await;
    }

    async fn timed<T, E: Display>(
        samples: &Mutex<ProofsSamples>,
        method: &'static str,
        call: impl IntoFuture<Output = Result<T, E>>,
    ) -> Option<T> {
        let started = Instant::now();
        let result = match tokio::time::timeout(PROBE_CALL_TIMEOUT, call.into_future()).await {
            Ok(result) => result.map_err(|error| error.to_string()),
            Err(elapsed) => Err(elapsed.to_string()),
        };
        let elapsed_ms = started.elapsed().as_secs_f64() * 1_000.0;
        let mut samples = samples.lock().unwrap();
        let (latencies, errors) = samples.methods.entry(method).or_default();
        match &result {
            Ok(_) => latencies.push(elapsed_ms),
            Err(error) => {
                *errors += 1;
                warn!(method, %error, "proofs probe call failed");
            }
        }
        result.ok()
    }

    /// Rebuilds a block's payload attributes as the proof host does for `debug_executePayload`.
    pub fn payload_attributes(block: <Base as Network>::BlockResponse) -> BasePayloadAttributes {
        let header = &block.header.inner;
        let mut attributes = BasePayloadAttributes::default();
        attributes.payload_attributes.timestamp = header.timestamp;
        attributes.payload_attributes.prev_randao = header.mix_hash;
        attributes.payload_attributes.suggested_fee_recipient = header.beneficiary;
        attributes.payload_attributes.parent_beacon_block_root = header.parent_beacon_block_root;
        attributes.payload_attributes.withdrawals =
            block.withdrawals.as_ref().map(|withdrawals| withdrawals.0.clone());
        attributes.no_tx_pool = Some(true);
        attributes.gas_limit = Some(header.gas_limit);
        // Replayed blocks are past Holocene, whose extra data is
        // `version || denominator || elasticity`, the `eip1559Params` bytes; Jovian appends
        // `minBaseFee`.
        attributes.eip_1559_params = header.extra_data.get(1..9).map(B64::from_slice);
        attributes.min_base_fee = header
            .extra_data
            .get(9..17)
            .and_then(|fee| fee.try_into().ok())
            .map(u64::from_be_bytes);
        attributes.transactions = Some(
            block
                .transactions
                .into_transactions()
                .map(|tx| tx.as_ref().encoded_2718().into())
                .collect(),
        );
        attributes
    }
}

#[cfg(test)]
mod tests {
    use alloy_eips::eip1559::BaseFeeParams;
    use alloy_network::Network;
    use alloy_primitives::B64;
    use base_common_consensus::JovianExtraData;
    use base_common_network::Base;

    use super::{CountSummary, ProofsSamples, ReplayProofsProbe};

    #[test]
    fn count_summary_uses_nearest_rank_percentiles() {
        let values = (1..=200).rev().collect::<Vec<u64>>();

        assert_eq!(
            CountSummary::from_values(&values),
            CountSummary { samples: 200, p50: 100, p99: 198, max: 200 }
        );
        assert_eq!(
            CountSummary::from_values(&[0, 3, 1]),
            CountSummary { samples: 3, p50: 1, p99: 3, max: 3 }
        );
        assert_eq!(CountSummary::from_values(&[]), CountSummary::default());
    }

    #[test]
    fn summary_reports_lag_witness_size_and_errors_per_method() {
        let mut samples = ProofsSamples {
            head_lag_blocks: vec![0, 3, 1],
            failed_probes: 2,
            witness_bytes: vec![4_096],
            ..Default::default()
        };
        samples.methods.insert("debug_executePayload", (vec![20.0, 40.0], 1));
        samples.methods.insert("eth_getProof", (Vec::new(), 2));

        let summary = samples.summarize();

        assert_eq!(summary.head_lag_blocks, CountSummary { samples: 3, p50: 1, p99: 3, max: 3 });
        assert_eq!(summary.failed_probes, 2);
        assert_eq!(summary.witness_bytes.max, 4_096);
        let execute = summary.methods["debug_executePayload"];
        assert_eq!(
            (execute.errors, execute.latency.max_ms, execute.latency.p50_ms),
            (1, 40.0, 20.0)
        );
        assert_eq!(summary.methods["eth_getProof"].errors, 2);
        assert_eq!(summary.methods["eth_getProof"].latency.max_ms, 0.0);
    }

    #[test]
    fn payload_attributes_carry_jovian_fee_parameters() {
        let params = B64::from([0, 0, 0, 250, 0, 0, 0, 6]);
        let mut block = <Base as Network>::BlockResponse::default();
        block.header.inner.gas_limit = 200_000_000;
        block.header.inner.extra_data =
            JovianExtraData::encode(params, BaseFeeParams::optimism(), 1_000_000).unwrap();

        let attributes = ReplayProofsProbe::payload_attributes(block.clone());

        assert_eq!(attributes.eip_1559_params, Some(params));
        assert_eq!(attributes.min_base_fee, Some(1_000_000));
        assert_eq!(attributes.gas_limit, Some(200_000_000));
        assert_eq!(attributes.no_tx_pool, Some(true));

        block.header.inner.extra_data = Default::default();
        let attributes = ReplayProofsProbe::payload_attributes(block);
        assert_eq!((attributes.eip_1559_params, attributes.min_base_fee), (None, None));
    }
}
