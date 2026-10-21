//! In-memory L1 simulator for deterministic actor-integration tests.
//!
//! `extend` derives one L2 block per L1 block and drives the engine and derivation actors with it,
//! allowing tests to drive consensus progression without real RPC or beacon nodes.

use std::sync::Arc;

use alloy_eips::BlockNumberOrTag;
use alloy_primitives::B256;
use base_common_genesis::RollupConfig;
use base_consensus_engine::test_utils::{
    TestAttributesBuilder, encoded_l1_info_deposit_tx, matching_rpc_block,
};
use base_protocol::{AttributesWithParent, BlockInfo, L1BlockInfoBedrock, L2BlockInfo};
use tokio::sync::mpsc;

use super::FakeEngineClientHandle;
use crate::{DerivationActorRequest, EngineActorRequest};

/// In-memory L1 simulator used by harness tests.
#[derive(Clone, Debug)]
pub struct FakeL1 {
    cfg: Arc<RollupConfig>,
    engine_request_tx: mpsc::Sender<EngineActorRequest>,
    derivation_request_tx: Option<mpsc::Sender<DerivationActorRequest>>,
    engine_handle: FakeEngineClientHandle,
}

impl FakeL1 {
    /// Creates a new fake L1 simulator.
    pub const fn new(
        cfg: Arc<RollupConfig>,
        engine_request_tx: mpsc::Sender<EngineActorRequest>,
        derivation_request_tx: Option<mpsc::Sender<DerivationActorRequest>>,
        engine_handle: FakeEngineClientHandle,
    ) -> Self {
        Self { cfg, engine_request_tx, derivation_request_tx, engine_handle }
    }

    /// Extends the chain by one block and drives the engine/derivation actors with the L2 block
    /// derived from it (see [`derive`](Self::derive)), which it returns.
    ///
    /// Each call consumes **two** scripted FCU responses: one synthetic (via
    /// `inject_fcu_v3_call`) and one real (from the engine actor processing
    /// `ProcessDerivedAttributesRequest`). Script the response queue with this in mind.
    ///
    /// The injected FCU call-log entry sets head==safe==finalized to the same hash, which is a
    /// deliberate simplification: the real protocol advances these three heads independently.
    /// Tests must therefore drive progress via the `ProcessDerivedAttributesRequest` channel and
    /// must NOT derive unsafe/finalized-head ordering from the call log.
    pub async fn extend(&self, block: BlockInfo) -> L2BlockInfo {
        let (attributes, safe_l2) = self.derive(block);

        self.engine_request_tx
            .send(EngineActorRequest::ProcessDerivedAttributesRequest(Box::new(attributes)))
            .await
            .expect("engine actor request channel closed while dispatching derived attributes");

        self.engine_handle.inject_fcu_v3_call(alloy_rpc_types_engine::ForkchoiceState {
            head_block_hash: safe_l2.block_info.hash,
            safe_block_hash: safe_l2.block_info.hash,
            finalized_block_hash: safe_l2.block_info.hash,
        });

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

        safe_l2
    }

    /// Derives the L2 block of `block` and scripts the fake engine to hold it, without driving
    /// the actors. Returns the derived attributes and the L2 block.
    pub fn derive(&self, block: BlockInfo) -> (AttributesWithParent, L2BlockInfo) {
        assert!(
            (1..=u8::MAX as u64).contains(&block.number),
            "the parent hash encodes the parent number into a single byte"
        );
        let parent = L2BlockInfo {
            block_info: BlockInfo {
                number: block.number - 1,
                hash: B256::from([(block.number - 1) as u8; 32]),
                ..Default::default()
            },
            ..Default::default()
        };
        let l1_info = L1BlockInfoBedrock::new_from_number_and_block_hash(block.number, block.hash);
        // Ending the span makes the engine issue an FCU in every consolidation path.
        let attributes = TestAttributesBuilder::new()
            .with_parent(parent)
            .with_timestamp(block.timestamp)
            .with_transactions(vec![encoded_l1_info_deposit_tx(l1_info)])
            .with_is_last_in_span(true)
            .build();
        let rpc_block = matching_rpc_block(&attributes);
        let safe_l2 = L2BlockInfo::from_block_and_genesis(
            &rpc_block.clone().into_consensus().map_transactions(|tx| tx.inner.inner.into_inner()),
            &self.cfg.genesis,
        )
        .expect("the derived block starts with an L1 info deposit");
        self.engine_handle.set_l2_block_by_label(
            BlockNumberOrTag::Number(block.number),
            rpc_block.map_header(Into::into),
        );

        (attributes, safe_l2)
    }
}
