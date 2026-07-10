//! In-memory L1 simulator for deterministic actor-integration tests.
//!
//! This fake supports scripted chain extension, reorgs, and transport stalls while providing
//! minimal trait seams used by derivation stack components (`BeaconClient` and
//! `L1RetrievalProvider`). `extend` emits synthetic safe-L2 block signals into the engine actor,
//! allowing tests to drive consensus progression without real RPC or beacon nodes.

use std::{collections::VecDeque, sync::Arc};

use alloy_primitives::{Address, B256};
use async_trait::async_trait;
use base_consensus_derive::{L1RetrievalProvider, PipelineError, PipelineResult};
use base_consensus_engine::ConsolidateInput;
use base_consensus_providers::{APIConfigResponse, APIGenesisResponse, BeaconClient, BoxedBlob};
use base_protocol::{BlockInfo, L2BlockInfo};
use tokio::sync::{Mutex, mpsc};

use crate::{DerivationActorRequest, EngineActorRequest};

use super::FakeEngineClientHandle;

/// Beacon-client error for [`FakeL1`].
#[derive(Clone, Debug)]
pub enum FakeL1BeaconError {
    /// The fake L1 is currently stalled.
    Stalled,
}

impl core::fmt::Display for FakeL1BeaconError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Stalled => write!(f, "fake l1 is stalled"),
        }
    }
}

/// Shared mutable state for [`FakeL1`].
#[derive(Clone, Debug, Default)]
pub struct FakeL1State {
    /// Canonical L1 chain blocks.
    pub canonical: Vec<BlockInfo>,
    /// Blocks queued while stalled.
    pub pending: VecDeque<BlockInfo>,
    /// Whether chain delivery is currently stalled.
    pub stalled: bool,
}

/// In-memory L1 simulator used by harness tests.
#[derive(Clone, Debug)]
pub struct FakeL1 {
    state: Arc<Mutex<FakeL1State>>,
    engine_request_tx: mpsc::Sender<EngineActorRequest>,
    derivation_request_tx: Option<mpsc::Sender<DerivationActorRequest>>,
    engine_handle: Option<FakeEngineClientHandle>,
    slot_interval: u64,
    genesis_time: u64,
}

impl FakeL1 {
    /// Creates a new fake L1 simulator.
    pub fn new(
        engine_request_tx: mpsc::Sender<EngineActorRequest>,
        derivation_request_tx: Option<mpsc::Sender<DerivationActorRequest>>,
        engine_handle: Option<FakeEngineClientHandle>,
    ) -> Self {
        Self {
            state: Arc::new(Mutex::new(FakeL1State::default())),
            engine_request_tx,
            derivation_request_tx,
            engine_handle,
            slot_interval: 12,
            genesis_time: 0,
        }
    }

    /// Returns a snapshot of the current fake L1 state.
    pub async fn state(&self) -> FakeL1State {
        self.state.lock().await.clone()
    }

    /// Extends the canonical chain by one block.
    pub async fn extend(&self, block: BlockInfo) {
        let mut state = self.state.lock().await;
        state.canonical.push(block);
        if state.stalled {
            state.pending.push_back(block);
            return;
        }
        drop(state);
        self.dispatch_safe_l2_for(block).await;
    }

    /// Reorgs the chain by dropping `depth` blocks and appending `alt_blocks`.
    pub async fn reorg(&self, depth: usize, alt_blocks: Vec<BlockInfo>) {
        let mut state = self.state.lock().await;
        let new_len = state.canonical.len().saturating_sub(depth);
        state.canonical.truncate(new_len);
        for block in &alt_blocks {
            state.canonical.push(*block);
        }
        let stalled = state.stalled;
        drop(state);

        if !stalled {
            for block in alt_blocks {
                self.dispatch_safe_l2_for(block).await;
            }
        }
    }

    /// Stalls chain delivery.
    pub async fn stall(&self) {
        self.state.lock().await.stalled = true;
    }

    /// Resumes delivery and flushes queued blocks.
    pub async fn resume(&self) {
        let mut state = self.state.lock().await;
        state.stalled = false;
        let pending = state.pending.drain(..).collect::<Vec<_>>();
        drop(state);

        for block in pending {
            self.dispatch_safe_l2_for(block).await;
        }
    }

    async fn dispatch_safe_l2_for(&self, block: BlockInfo) {
        let parent_hash = if block.number == 0 {
            B256::ZERO
        } else {
            B256::from([block.number.saturating_sub(1) as u8; 32])
        };
        let safe_l2 = L2BlockInfo {
            block_info: BlockInfo {
                number: block.number,
                hash: B256::from([block.number as u8; 32]),
                parent_hash,
                timestamp: block.timestamp,
            },
            l1_origin: block.id(),
            seq_num: block.number,
        };

        let send_result = self
            .engine_request_tx
            .send(EngineActorRequest::ProcessSafeL2SignalRequest(ConsolidateInput::BlockInfo(
                safe_l2,
            )))
            .await;

        if let Err(error) = send_result {
            warn!(target: "test_utils::fake_l1", error = ?error, "failed to dispatch safe l2 signal");
        }

        if let Some(engine_handle) = &self.engine_handle {
            engine_handle
                .inject_fcu_v3_call(alloy_rpc_types_engine::ForkchoiceState {
                    head_block_hash: safe_l2.block_info.hash,
                    safe_block_hash: safe_l2.block_info.hash,
                    finalized_block_hash: safe_l2.block_info.hash,
                })
                .await;
        }

        if let Some(derivation_request_tx) = &self.derivation_request_tx {
            let update =
                DerivationActorRequest::ProcessEngineSafeHeadUpdateRequest(Box::new(safe_l2));
            if let Err(error) = derivation_request_tx.send(update).await {
                warn!(target: "test_utils::fake_l1", error = ?error, "failed to dispatch derivation safe-head update");
            }
        }
    }
}

#[async_trait]
impl BeaconClient for FakeL1 {
    type Error = FakeL1BeaconError;

    fn slot_not_found(_err: &Self::Error) -> Option<u64> {
        None
    }

    async fn slot_interval(&self) -> Result<APIConfigResponse, Self::Error> {
        Ok(APIConfigResponse::new(self.slot_interval))
    }

    async fn genesis_time(&self) -> Result<APIGenesisResponse, Self::Error> {
        Ok(APIGenesisResponse::new(self.genesis_time))
    }

    async fn filtered_beacon_blobs(
        &self,
        _slot: u64,
        _blob_hashes: &[B256],
    ) -> Result<Vec<BoxedBlob>, Self::Error> {
        if self.state.lock().await.stalled {
            return Err(FakeL1BeaconError::Stalled);
        }
        Ok(Vec::new())
    }
}

#[async_trait]
impl L1RetrievalProvider for FakeL1 {
    async fn next_l1_block(&mut self) -> PipelineResult<Option<BlockInfo>> {
        let mut state = self.state.lock().await;
        if state.stalled {
            return Err(PipelineError::Eof.temp());
        }
        Ok(state.pending.pop_front())
    }

    fn batcher_addr(&self) -> Address {
        Address::ZERO
    }
}
