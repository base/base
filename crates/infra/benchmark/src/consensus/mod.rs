//! Consensus clients: sequencer (block proposer) and syncing (validator replay).

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use alloy_primitives::{Address, Bytes, TxKind, B256, B64, U256};
use alloy_provider::RootProvider;
use alloy_rpc_types_engine::{ForkchoiceState, PayloadAttributes, PayloadId};
use base_common_consensus::{TxDeposit, DEPOSIT_TX_TYPE_ID};
use base_common_genesis::RollupConfig;
use base_common_network::{Base, BaseEngineApi};
use base_common_rpc_types_engine::{BaseExecutionPayloadV4, BasePayloadAttributes};
use base_consensus_engine::{BaseEngineClient, EngineClientBuilder};
use reqwest::Url;
use tokio::task::JoinSet;
use tracing::info;

use crate::error::BenchmarkError;
use crate::metrics::BlockMetrics;
use crate::params;

const FAKE_BEACON_ROOT_PREIMAGE: &[u8] = b"fake-beacon-block-root\x01";

fn fake_beacon_root() -> B256 {
    use alloy_primitives::keccak256;
    keccak256(FAKE_BEACON_ROOT_PREIMAGE)
}

fn holocene_eip1559_params() -> B64 {
    let mut bytes = [0u8; 8];
    bytes[0..4].copy_from_slice(&(params::EIP1559_ELASTICITY as u32).to_be_bytes());
    bytes[4..8].copy_from_slice(&(params::EIP1559_DENOMINATOR as u32).to_be_bytes());
    B64::from(bytes)
}

/// Pending transaction pool shared between the payload worker and consensus client.
///
/// Deposit transactions (first byte `0x7E`) are moved to the front of the drain
/// result so they are always included before regular transactions.
#[derive(Debug, Clone, Default)]
pub struct FakeMempool {
    pending: Arc<Mutex<VecDeque<Bytes>>>,
}

impl FakeMempool {
    /// Create a new empty mempool.
    pub fn new() -> Self {
        Self::default()
    }

    /// Append transactions to the pending queue.
    pub fn add_transactions(&self, txs: Vec<Bytes>) {
        let mut q = self.pending.lock().unwrap();
        q.extend(txs);
    }

    /// Drain all pending transactions, deposits first.
    pub fn drain(&self) -> Vec<Bytes> {
        let mut q = self.pending.lock().unwrap();
        let (mut deposits, mut regular): (Vec<Bytes>, Vec<Bytes>) =
            q.drain(..).partition(|tx| tx.first().copied() == Some(0x7E));
        deposits.append(&mut regular);
        deposits
    }

    /// Number of pending transactions.
    pub fn len(&self) -> usize {
        self.pending.lock().unwrap().len()
    }

    /// Returns `true` if there are no pending transactions.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

type EngineClient = BaseEngineClient<RootProvider, RootProvider<Base>>;

/// Thin wrapper over [`BaseEngineClient`] providing timeout-bounded Engine API
/// calls and head tracking.
pub struct BaseConsensusClient {
    engine: EngineClient,
    pub head_block_hash: B256,
    pub head_block_number: u64,
    pub head_block_timestamp: u64,
}

impl BaseConsensusClient {
    /// Connect to the Engine API at `auth_url` using the provided JWT secret
    /// and rollup config. A dummy L1 URL is used since the benchmark has no
    /// real L1 chain.
    pub async fn connect(
        auth_url: Url,
        jwt_secret: alloy_rpc_types_engine::JwtSecret,
        cfg: Arc<RollupConfig>,
    ) -> Result<Self, BenchmarkError> {
        let dummy_l1: Url = "http://127.0.0.1:1".parse().unwrap();
        let engine = EngineClientBuilder { l2: auth_url, l2_jwt: jwt_secret, l1_rpc: dummy_l1, cfg }
            .build()
            .await
            .map_err(|e| BenchmarkError::EngineApi(format!("failed to connect engine client: {e}")))?;
        Ok(Self { engine, head_block_hash: B256::ZERO, head_block_number: 0, head_block_timestamp: 0 })
    }

    /// `engine_forkchoiceUpdatedV3` with a 10-second timeout.
    pub async fn update_fork_choice(
        &mut self,
        attrs: Option<BasePayloadAttributes>,
    ) -> Result<Option<PayloadId>, BenchmarkError> {
        let state = ForkchoiceState {
            head_block_hash: self.head_block_hash,
            safe_block_hash: self.head_block_hash,
            finalized_block_hash: self.head_block_hash,
        };
        let result = tokio::time::timeout(
            Duration::from_secs(10),
            self.engine.fork_choice_updated_v3(state, attrs),
        )
        .await
        .map_err(|_| BenchmarkError::Timeout("fork_choice_updated_v3 timed out".into()))?
        .map_err(|e| BenchmarkError::EngineApi(format!("fork_choice_updated_v3 failed: {e}")))?;
        Ok(result.payload_id)
    }

    /// `engine_getPayloadV4` with a 240-second timeout.
    pub async fn get_built_payload(
        &self,
        payload_id: PayloadId,
    ) -> Result<BaseExecutionPayloadV4, BenchmarkError> {
        let envelope = tokio::time::timeout(
            Duration::from_secs(240),
            self.engine.get_payload_v4(payload_id),
        )
        .await
        .map_err(|_| BenchmarkError::Timeout("get_payload_v4 timed out".into()))?
        .map_err(|e| BenchmarkError::EngineApi(format!("get_payload_v4 failed: {e}")))?;
        Ok(envelope.execution_payload)
    }

    /// `engine_newPayloadV4` with a 30-second timeout. Updates head on success.
    pub async fn new_payload(
        &mut self,
        payload: BaseExecutionPayloadV4,
        beacon_root: B256,
    ) -> Result<(), BenchmarkError> {
        let block_hash = payload.payload_inner.payload_inner.payload_inner.block_hash;
        let block_number = payload.payload_inner.payload_inner.payload_inner.block_number;
        let timestamp = payload.payload_inner.payload_inner.payload_inner.timestamp;

        tokio::time::timeout(
            Duration::from_secs(30),
            self.engine.new_payload_v4(payload, beacon_root),
        )
        .await
        .map_err(|_| BenchmarkError::Timeout("new_payload_v4 timed out".into()))?
        .map_err(|e| BenchmarkError::EngineApi(format!("new_payload_v4 failed: {e}")))?;

        self.head_block_hash = block_hash;
        self.head_block_number = block_number;
        self.head_block_timestamp = timestamp;
        Ok(())
    }
}

/// Drives the sequencer: sends transactions, proposes blocks via Engine API.
pub struct SequencerConsensusClient {
    base: BaseConsensusClient,
    rpc_url: String,
}

impl SequencerConsensusClient {
    /// Create from an already-connected [`BaseConsensusClient`].
    pub fn new(base: BaseConsensusClient, rpc_url: String) -> Self {
        Self { base, rpc_url }
    }

    /// Propose one block: drain mempool → batch-send txs → FCU → sleep →
    /// getPayload → newPayload. Returns the built payload and timing metrics.
    pub async fn propose(
        &mut self,
        mempool: &FakeMempool,
        block_time: Duration,
        gas_limit: u64,
    ) -> Result<(BaseExecutionPayloadV4, BlockMetrics), BenchmarkError> {
        let txs = mempool.drain();

        let send_start = Instant::now();
        self.batch_send_txs(&txs).await?;
        let send_latency = send_start.elapsed();

        let attrs = self.build_payload_attributes(gas_limit)?;

        let fcu_start = Instant::now();
        let payload_id = self
            .base
            .update_fork_choice(Some(attrs))
            .await?
            .ok_or_else(|| BenchmarkError::EngineApi("FCU returned no payload_id".into()))?;
        let fcu_latency = fcu_start.elapsed();

        tokio::time::sleep(block_time).await;

        let get_start = Instant::now();
        let payload = self.base.get_built_payload(payload_id).await?;
        let get_latency = get_start.elapsed();

        let beacon_root = fake_beacon_root();
        self.base.new_payload(payload.clone(), beacon_root).await?;

        let gas_used = payload.payload_inner.payload_inner.payload_inner.gas_used;
        let tx_count = payload.payload_inner.payload_inner.payload_inner.transactions.len() as u64;

        info!(
            block = %self.base.head_block_number,
            gas = %gas_used,
            txs = %tx_count,
            fcu_ms = %fcu_latency.as_millis(),
            get_ms = %get_latency.as_millis(),
            "block proposed",
        );

        let mut metrics = BlockMetrics::new(self.base.head_block_number);
        metrics.add_execution_metric(crate::metrics::SEND_TXS_LATENCY, send_latency.as_secs_f64());
        metrics.add_execution_metric(crate::metrics::UPDATE_FORK_CHOICE_LATENCY, fcu_latency.as_secs_f64());
        metrics.add_execution_metric(crate::metrics::GET_PAYLOAD_LATENCY, get_latency.as_secs_f64());
        metrics.add_execution_metric(crate::metrics::GAS_PER_BLOCK, gas_used as f64);
        metrics.add_execution_metric(crate::metrics::TRANSACTIONS_PER_BLOCK, tx_count as f64);

        Ok((payload, metrics))
    }

    async fn batch_send_txs(&self, txs: &[Bytes]) -> Result<(), BenchmarkError> {
        const BATCH_SIZE: usize = 100;
        let client = reqwest::Client::new();
        let mut set = JoinSet::new();

        for batch in txs.chunks(BATCH_SIZE) {
            let batch: Vec<serde_json::Value> = batch
                .iter()
                .map(|tx| {
                    serde_json::json!({
                        "jsonrpc": "2.0",
                        "method": "eth_sendRawTransaction",
                        "params": [format!("0x{}", hex::encode(tx))],
                        "id": 1,
                    })
                })
                .collect();

            let url = self.rpc_url.clone();
            let c = client.clone();
            set.spawn(async move {
                let _ = c.post(&url).json(&batch).send().await;
            });
        }

        while set.join_next().await.is_some() {}
        Ok(())
    }

    fn build_payload_attributes(&self, gas_limit: u64) -> Result<BasePayloadAttributes, BenchmarkError> {
        let timestamp = self.base.head_block_timestamp + 1;
        let beacon_root = fake_beacon_root();

        let deposit_tx = build_l1_info_deposit_tx();

        Ok(BasePayloadAttributes {
            payload_attributes: PayloadAttributes {
                timestamp,
                prev_randao: B256::ZERO,
                suggested_fee_recipient: params::SUGGESTED_FEE_RECIPIENT,
                withdrawals: Some(vec![]),
                parent_beacon_block_root: Some(beacon_root),
            },
            transactions: Some(vec![deposit_tx]),
            no_tx_pool: Some(false),
            gas_limit: Some(gas_limit),
            eip_1559_params: Some(holocene_eip1559_params()),
            min_base_fee: Some(1),
        })
    }
}

/// Replays a list of payloads through the Engine API on a validator node.
pub struct SyncingConsensusClient {
    base: BaseConsensusClient,
}

impl SyncingConsensusClient {
    /// Create from an already-connected [`BaseConsensusClient`].
    pub fn new(base: BaseConsensusClient) -> Self {
        Self { base }
    }

    /// Replay `payloads` via newPayload + FCU, collecting metrics for blocks
    /// at or above `first_test_block`.
    pub async fn start(
        &mut self,
        payloads: &[BaseExecutionPayloadV4],
        first_test_block: u64,
        block_time: Duration,
    ) -> Result<Vec<BlockMetrics>, BenchmarkError> {
        let beacon_root = fake_beacon_root();
        let mut collected = Vec::new();

        for payload in payloads {
            let block_number =
                payload.payload_inner.payload_inner.payload_inner.block_number;

            let new_payload_start = Instant::now();
            self.base.new_payload(payload.clone(), beacon_root).await?;
            let new_payload_latency = new_payload_start.elapsed();

            let fcu_start = Instant::now();
            self.base.update_fork_choice(None).await?;
            let fcu_latency = fcu_start.elapsed();

            tokio::time::sleep(block_time).await;

            if block_number >= first_test_block {
                let gas_used =
                    payload.payload_inner.payload_inner.payload_inner.gas_used;
                let tx_count =
                    payload.payload_inner.payload_inner.payload_inner.transactions.len() as u64;

                let mut metrics = BlockMetrics::new(block_number);
                metrics.add_execution_metric(crate::metrics::NEW_PAYLOAD_LATENCY, new_payload_latency.as_secs_f64());
                metrics.add_execution_metric(crate::metrics::UPDATE_FORK_CHOICE_LATENCY, fcu_latency.as_secs_f64());
                metrics.add_execution_metric(crate::metrics::GAS_PER_BLOCK, gas_used as f64);
                metrics.add_execution_metric(crate::metrics::TRANSACTIONS_PER_BLOCK, tx_count as f64);
                collected.push(metrics);
            }
        }

        Ok(collected)
    }
}

/// Construct an all-zero L1 block info deposit transaction (Jovian format).
///
/// Required in every payload's transaction list. Since the benchmark has no
/// real L1 chain, all L1 fields are zero.
fn build_l1_info_deposit_tx() -> Bytes {
    use alloy_rlp::Encodable;

    let deposit = TxDeposit {
        source_hash: B256::ZERO,
        from: Address::ZERO,
        to: TxKind::Call(Address::ZERO),
        mint: 0,
        value: U256::ZERO,
        gas_limit: 1_000_000,
        is_system_transaction: true,
        input: Bytes::from(vec![0u8; 64]),
    };

    let mut rlp_buf = Vec::new();
    deposit.encode(&mut rlp_buf);

    let mut out = Vec::with_capacity(1 + rlp_buf.len());
    out.push(DEPOSIT_TX_TYPE_ID);
    out.extend_from_slice(&rlp_buf);
    Bytes::from(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fake_mempool_drain_deposits_first() {
        let pool = FakeMempool::new();
        let regular = Bytes::from(vec![0x02, 0xaa, 0xbb]);
        let deposit = Bytes::from(vec![0x7E, 0x01, 0x02]);
        pool.add_transactions(vec![regular.clone(), deposit.clone()]);
        let drained = pool.drain();
        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0], deposit);
        assert_eq!(drained[1], regular);
    }

    #[test]
    fn fake_mempool_drain_clears_queue() {
        let pool = FakeMempool::new();
        pool.add_transactions(vec![Bytes::from(vec![0x01])]);
        let _ = pool.drain();
        assert!(pool.is_empty());
    }

    #[test]
    fn fake_mempool_multiple_deposits_before_regular() {
        let pool = FakeMempool::new();
        let r1 = Bytes::from(vec![0x02]);
        let d1 = Bytes::from(vec![0x7E, 0x01]);
        let r2 = Bytes::from(vec![0x03]);
        let d2 = Bytes::from(vec![0x7E, 0x02]);
        pool.add_transactions(vec![r1.clone(), d1.clone(), r2.clone(), d2.clone()]);
        let drained = pool.drain();
        assert_eq!(&drained[0..2], &[d1, d2]);
        assert_eq!(&drained[2..4], &[r1, r2]);
    }

    #[test]
    fn holocene_eip1559_params_encoding() {
        let params = holocene_eip1559_params();
        let bytes = params.as_slice();
        let elasticity = u32::from_be_bytes(bytes[0..4].try_into().unwrap());
        let denominator = u32::from_be_bytes(bytes[4..8].try_into().unwrap());
        assert_eq!(elasticity, 50);
        assert_eq!(denominator, 1);
    }

    #[test]
    fn fake_beacon_root_is_deterministic() {
        assert_eq!(fake_beacon_root(), fake_beacon_root());
    }

    #[test]
    fn deposit_tx_has_type_prefix() {
        let tx = build_l1_info_deposit_tx();
        assert_eq!(tx.first().copied(), Some(0x7E));
    }

    #[test]
    fn deposit_tx_non_empty() {
        let tx = build_l1_info_deposit_tx();
        assert!(tx.len() > 10);
    }
}
