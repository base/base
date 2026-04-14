//! Tests for [`DelegatedForkchoiceTask::execute`].

use std::sync::Arc;

use alloy_eips::BlockNumberOrTag;
use alloy_primitives::B256;
use alloy_rpc_types_engine::{ForkchoiceUpdated, PayloadStatus, PayloadStatusEnum};
use alloy_rpc_types_eth::Block as RpcBlock;
use base_alloy_rpc_types::Transaction as OpTransaction;
use base_consensus_genesis::RollupConfig;
use base_protocol::{BlockInfo, L2BlockInfo};

use crate::{
    DelegatedForkchoiceTask, DelegatedForkchoiceUpdate, EngineTaskExt,
    test_utils::{TestEngineStateBuilder, test_block_info, test_engine_client_builder},
};

fn syncing_fcu() -> ForkchoiceUpdated {
    ForkchoiceUpdated {
        payload_status: PayloadStatus {
            status: PayloadStatusEnum::Syncing,
            latest_valid_hash: None,
        },
        payload_id: None,
    }
}

fn block_with_hash(number: u64, hash: B256) -> RpcBlock<OpTransaction> {
    let mut block = RpcBlock::<OpTransaction>::default();
    block.header.hash = hash;
    block.header.inner.number = number;
    block.header.inner.timestamp = number * 2;
    block
}

#[tokio::test]
async fn syncing_safe_update_skips_finalization_beyond_actual_safe() {
    let delegated_safe_number = 80;
    let delegated_safe_hash = B256::from([0x11; 32]);
    let delegated_safe = L2BlockInfo {
        block_info: BlockInfo {
            hash: delegated_safe_hash,
            number: delegated_safe_number,
            ..Default::default()
        },
        ..Default::default()
    };

    let client = Arc::new(
        test_engine_client_builder()
            .with_l2_block_by_label(
                BlockNumberOrTag::Number(delegated_safe_number),
                block_with_hash(delegated_safe_number, delegated_safe_hash),
            )
            .with_fork_choice_updated_v3_response(syncing_fcu())
            .build(),
    );

    let mut state = TestEngineStateBuilder::new()
        .with_unsafe_head(test_block_info(100))
        .with_safe_head(L2BlockInfo::default())
        .with_finalized_head(L2BlockInfo::default())
        .with_el_sync_finished(false)
        .build();

    let task = DelegatedForkchoiceTask::new(
        client,
        Arc::new(RollupConfig::default()),
        DelegatedForkchoiceUpdate {
            safe_l2: delegated_safe,
            finalized_l2_number: Some(delegated_safe_number),
        },
    );

    task.execute(&mut state).await.expect("delegated forkchoice should not fail");

    assert_eq!(
        state.sync_state.safe_head(),
        L2BlockInfo::default(),
        "safe head must remain unchanged when safe FCU returns Syncing",
    );
    assert_eq!(
        state.sync_state.finalized_head(),
        L2BlockInfo::default(),
        "finalized head must not advance past the actual safe head",
    );
}
