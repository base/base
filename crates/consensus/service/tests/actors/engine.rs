//! Integration tests for the engine processing path.

use std::sync::Arc;

use alloy_primitives::B256;
use alloy_rpc_types_engine::{ForkchoiceUpdated, PayloadStatus, PayloadStatusEnum};
use alloy_rpc_types_eth::Block as RpcBlock;
use async_trait::async_trait;
use base_common_rpc_types::Transaction as OpTransaction;
use base_consensus_engine::{
    DelegatedForkchoiceUpdate, Engine,
    test_utils::{TestEngineStateBuilder, test_block_info, test_engine_client_builder},
};
use base_consensus_genesis::RollupConfig;
use base_consensus_node::{
    EngineDerivationClient, EngineError, EngineProcessingRequest, EngineProcessor,
    EngineRequestReceiver,
};
use base_protocol::{BlockInfo, L2BlockInfo};
use tokio::sync::{mpsc, watch};

#[derive(Debug, Default)]
struct NoopDerivationClient;

#[async_trait]
impl EngineDerivationClient for NoopDerivationClient {
    async fn notify_sync_completed(
        &self,
        _: L2BlockInfo,
    ) -> Result<(), base_consensus_node::DerivationClientError> {
        Ok(())
    }

    async fn send_new_engine_safe_head(
        &self,
        _: L2BlockInfo,
    ) -> Result<(), base_consensus_node::DerivationClientError> {
        Ok(())
    }

    async fn send_signal(
        &self,
        _: base_consensus_derive::Signal,
    ) -> Result<(), base_consensus_node::DerivationClientError> {
        Ok(())
    }
}

const fn syncing_fcu() -> ForkchoiceUpdated {
    ForkchoiceUpdated {
        payload_status: PayloadStatus {
            status: PayloadStatusEnum::Syncing,
            latest_valid_hash: None,
        },
        payload_id: None,
    }
}

fn mismatched_block(number: u64) -> RpcBlock<OpTransaction> {
    let mut block = RpcBlock::<OpTransaction>::default();
    block.header.hash = B256::from([0xabu8; 32]);
    block.header.inner.number = number;
    block.header.inner.timestamp = number * 2;
    block
}

#[tokio::test(flavor = "multi_thread")]
async fn follow_restart_delegated_forkchoice_does_not_finalize_past_actual_safe_head() {
    let unsafe_head = test_block_info(100);
    let delegated_safe_number = 80;

    let initial_state = TestEngineStateBuilder::new()
        .with_unsafe_head(unsafe_head)
        .with_safe_head(L2BlockInfo::default())
        .with_finalized_head(L2BlockInfo::default())
        .with_el_sync_finished(false)
        .build();

    let client = Arc::new(
        test_engine_client_builder()
            .with_block_info_by_tag(alloy_eips::BlockNumberOrTag::Latest, unsafe_head)
            .with_l2_block_by_label(
                alloy_eips::BlockNumberOrTag::Number(delegated_safe_number),
                mismatched_block(delegated_safe_number),
            )
            .with_fork_choice_updated_v3_response(syncing_fcu())
            .build(),
    );

    let delegated_safe = L2BlockInfo {
        block_info: BlockInfo {
            number: delegated_safe_number,
            hash: B256::from([0xcdu8; 32]),
            ..Default::default()
        },
        ..Default::default()
    };

    let (state_tx, state_rx) = watch::channel(initial_state);
    let (queue_tx, _) = watch::channel(0usize);
    let engine = Engine::new(initial_state, state_tx, queue_tx);

    let processor = EngineProcessor::new(
        Arc::clone(&client),
        Arc::new(RollupConfig::default()),
        NoopDerivationClient,
        engine,
        None,
        None,
        false,
    );

    let (req_tx, req_rx) = mpsc::channel(8);
    let handle = processor.start(req_rx);

    state_rx
        .clone()
        .wait_for(|state| {
            state.sync_state.unsafe_head().block_info.number == unsafe_head.block_info.number
        })
        .await
        .expect("bootstrap did not seed unsafe head");

    req_tx
        .send(EngineProcessingRequest::ProcessDelegatedForkchoiceUpdate(Box::new(
            DelegatedForkchoiceUpdate {
                safe_l2: delegated_safe,
                finalized_l2_number: Some(delegated_safe_number),
            },
        )))
        .await
        .expect("failed to send delegated forkchoice update");

    drop(req_tx);
    let result = handle.await.expect("processor task panicked");
    assert!(
        matches!(result, Err(EngineError::ChannelClosed)),
        "expected ChannelClosed after request channel shutdown, got {result:?}"
    );

    let state = *state_rx.borrow();
    assert_eq!(
        state.sync_state.safe_head(),
        L2BlockInfo::default(),
        "safe head should remain unchanged when the delegated safe FCU returns Syncing",
    );
    assert_eq!(
        state.sync_state.finalized_head(),
        L2BlockInfo::default(),
        "finalized head must not advance past the actual engine safe head",
    );
}
