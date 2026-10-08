//! In-memory L1 simulator for deterministic actor-integration tests.
//!
//! `extend` emits synthetic safe-L2 block signals into the engine and derivation actors, allowing
//! tests to drive consensus progression without real RPC or beacon nodes.

use alloy_primitives::B256;
use base_consensus_engine::ConsolidateInput;
use base_protocol::{BlockInfo, L2BlockInfo};
use tokio::sync::mpsc;

use super::FakeEngineClientHandle;
use crate::{DerivationActorRequest, EngineActorRequest};

/// In-memory L1 simulator used by harness tests.
#[derive(Clone, Debug)]
pub struct FakeL1 {
    engine_request_tx: mpsc::Sender<EngineActorRequest>,
    derivation_request_tx: Option<mpsc::Sender<DerivationActorRequest>>,
    engine_handle: Option<FakeEngineClientHandle>,
}

impl FakeL1 {
    /// Creates a new fake L1 simulator.
    pub fn new(
        engine_request_tx: mpsc::Sender<EngineActorRequest>,
        derivation_request_tx: Option<mpsc::Sender<DerivationActorRequest>>,
        engine_handle: Option<FakeEngineClientHandle>,
    ) -> Self {
        Self { engine_request_tx, derivation_request_tx, engine_handle }
    }

    /// Extends the chain by one block and drives the engine/derivation actors for its safe-L2
    /// signal.
    ///
    /// Each call consumes **two** scripted FCU responses: one synthetic (via
    /// `inject_fcu_v3_call`) and one real (from the engine actor processing
    /// `ProcessSafeL2SignalRequest`). Script the response queue with this in mind.
    ///
    /// The injected FCU call-log entry sets head==safe==finalized to the same hash, which is a
    /// deliberate simplification: the real protocol advances these three heads independently.
    /// Tests must therefore drive progress via the `ProcessSafeL2SignalRequest` channel and must
    /// NOT derive unsafe/finalized-head ordering from the call log.
    pub async fn extend(&self, block: BlockInfo) {
        assert!(
            block.number <= u8::MAX as u64,
            "fake block hash encoding truncates the number into a single byte and wraps above 255"
        );
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

        self.engine_request_tx
            .send(EngineActorRequest::ProcessSafeL2SignalRequest(ConsolidateInput::BlockInfo(
                safe_l2,
            )))
            .await
            .expect("engine actor request channel closed while dispatching safe l2 signal");

        if let Some(engine_handle) = &self.engine_handle {
            engine_handle.inject_fcu_v3_call(alloy_rpc_types_engine::ForkchoiceState {
                head_block_hash: safe_l2.block_info.hash,
                safe_block_hash: safe_l2.block_info.hash,
                finalized_block_hash: safe_l2.block_info.hash,
            });
        }

        // Intentional ordering shortcut: in production, the derivation actor receives
        // ProcessEngineSafeHeadUpdateRequest only after the engine completes consolidation and
        // emits the update itself. Here we dispatch both simultaneously so tests do not need
        // to wait for the engine round-trip to observe safe-head advancement in derivation.
        if let Some(derivation_request_tx) = &self.derivation_request_tx {
            let update =
                DerivationActorRequest::ProcessEngineSafeHeadUpdateRequest(Box::new(safe_l2));
            derivation_request_tx.send(update).await.expect(
                "derivation actor request channel closed while dispatching safe-head update",
            );
        }
    }
}
