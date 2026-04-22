//! Tests for [`FinalizeTask::execute`].

use std::sync::Arc;

use alloy_eips::{BlockId, BlockNumHash, BlockNumberOrTag};
use alloy_primitives::{B256, b256};
use alloy_rpc_types_engine::{ForkchoiceUpdated, PayloadStatus, PayloadStatusEnum};
use alloy_rpc_types_eth::Block as RpcBlock;
use base_common_genesis::{ChainGenesis, RollupConfig};
use base_common_rpc_types::Transaction as BaseTransaction;

use crate::{
    EngineTaskExt, FinalizeTask, FinalizeTaskError,
    test_utils::{TestEngineStateBuilder, test_block_info, test_engine_client_builder},
};

/// The genesis block hash for Base Sepolia (block 0).
const BASE_SEPOLIA_GENESIS_HASH: B256 =
    b256!("0dcc9e089e30b90ddfc55be9a37dd15bc551aeee999d2e2b51414c54eaf934e4");

/// The genesis block hash for Base Mainnet (block 0).
const BASE_MAINNET_GENESIS_HASH: B256 =
    b256!("f712aa9241cc24369b143cf6dce85f0902a9731e70d66818a3a5845b296c73dd");

/// Construct a minimal default genesis block for testing [`FinalizeTask`].
///
/// Returns a default all-zero RPC block (number = 0, no transactions) paired with
/// the canonical hash produced by `hash_slow()` on its consensus form. Use the
/// returned hash as `genesis.l2.hash` in the test rollup config so that
/// [`L2BlockInfo::from_block_and_genesis`] accepts the block via the genesis path.
///
/// [`L2BlockInfo::from_block_and_genesis`]: base_protocol::L2BlockInfo::from_block_and_genesis
fn make_genesis_block() -> (RpcBlock<BaseTransaction>, B256) {
    let block = RpcBlock::<BaseTransaction>::default();
    let hash = block.clone().into_consensus().hash_slow();
    (block, hash)
}

/// Build a [`RollupConfig`] whose genesis L2 block number is 0 and hash is `hash`.
fn genesis_rollup_cfg(hash: B256) -> Arc<RollupConfig> {
    Arc::new(RollupConfig {
        genesis: ChainGenesis { l2: BlockNumHash { number: 0, hash }, ..Default::default() },
        ..Default::default()
    })
}

fn valid_fcu(hash: B256) -> ForkchoiceUpdated {
    ForkchoiceUpdated {
        payload_status: PayloadStatus {
            status: PayloadStatusEnum::Valid,
            latest_valid_hash: Some(hash),
        },
        payload_id: None,
    }
}

#[tokio::test]
async fn block_not_safe_returns_error() {
    // safe_head = 5, block_number = 10 → task fails before fetching the block.
    let client = test_engine_client_builder().build();
    let head = test_block_info(5);
    let mut state =
        TestEngineStateBuilder::new().with_safe_head(head).with_unsafe_head(head).build();

    let task = FinalizeTask::new(Arc::new(client), Arc::new(RollupConfig::default()), 10);
    let result = task.execute(&mut state).await;

    assert!(
        matches!(result, Err(FinalizeTaskError::BlockNotSafe)),
        "expected BlockNotSafe, got {result:?}"
    );
}

#[tokio::test]
async fn block_not_found_returns_error() {
    // safe_head = 10, block_number = 7, no block registered → mock returns None.
    let client = test_engine_client_builder().build();
    let head = test_block_info(10);
    let mut state =
        TestEngineStateBuilder::new().with_safe_head(head).with_unsafe_head(head).build();

    let task = FinalizeTask::new(Arc::new(client), Arc::new(RollupConfig::default()), 7);
    let result = task.execute(&mut state).await;

    assert!(
        matches!(result, Err(FinalizeTaskError::BlockNotFound(7))),
        "expected BlockNotFound(7), got {result:?}"
    );
}

#[tokio::test]
async fn from_block_error_on_genesis_hash_mismatch() {
    // Configure genesis.l2.hash = BASE_SEPOLIA_GENESIS_HASH but provide a default
    // all-zero block, whose hash_slow() will not equal the real Base Sepolia genesis
    // hash. from_block_and_genesis returns InvalidGenesisHash → FinalizeTaskError::FromBlock.
    let (block, _) = make_genesis_block();
    let cfg = genesis_rollup_cfg(BASE_SEPOLIA_GENESIS_HASH);

    let client = test_engine_client_builder()
        .with_config(Arc::clone(&cfg))
        .with_l2_block(BlockId::Number(BlockNumberOrTag::Number(0)), block)
        .build();

    let head = test_block_info(0);
    let mut state =
        TestEngineStateBuilder::new().with_safe_head(head).with_unsafe_head(head).build();

    let task = FinalizeTask::new(Arc::new(client), cfg, 0);
    let result = task.execute(&mut state).await;

    assert!(
        matches!(result, Err(FinalizeTaskError::FromBlock(_))),
        "expected FromBlock on genesis hash mismatch, got {result:?}"
    );
}

#[tokio::test]
async fn fcu_failure_propagates_as_forkchoice_update_failed() {
    // Provide a valid genesis block and matching config but do NOT configure a FCU
    // response. SynchronizeTask fails with a transport error → ForkchoiceUpdateFailed.
    // The FCU call uses the Base Sepolia genesis hash in the forkchoice state.
    let (block, hash) = make_genesis_block();
    let cfg = genesis_rollup_cfg(hash);

    let client = test_engine_client_builder()
        .with_config(Arc::clone(&cfg))
        .with_l2_block(BlockId::Number(BlockNumberOrTag::Number(0)), block)
        // fork_choice_updated_v3 intentionally NOT configured → transport error.
        .build();

    let mut state = TestEngineStateBuilder::new().build();

    let task = FinalizeTask::new(Arc::new(client), cfg, 0);
    let result = task.execute(&mut state).await;

    assert!(
        matches!(result, Err(FinalizeTaskError::ForkchoiceUpdateFailed(_))),
        "expected ForkchoiceUpdateFailed, got {result:?}"
    );
}

#[tokio::test]
async fn success_updates_engine_state_finalized_head() {
    // Full happy path: fetch the genesis block, pass from_block_and_genesis, dispatch
    // FCU, and verify the engine state updates. The Base Mainnet genesis hash is used
    // in the FCU valid response to confirm the correct block was finalized.
    let (block, hash) = make_genesis_block();
    let cfg = genesis_rollup_cfg(hash);

    let client = test_engine_client_builder()
        .with_config(Arc::clone(&cfg))
        .with_l2_block(BlockId::Number(BlockNumberOrTag::Number(0)), block)
        .with_fork_choice_updated_v3_response(valid_fcu(BASE_MAINNET_GENESIS_HASH))
        .build();

    // Default TestEngineStateBuilder starts with finalized_head.hash = B256::ZERO.
    // The computed genesis hash differs, so SynchronizeTask sees a state change and
    // calls FCU. After execution the finalized_head must reflect the new block.
    let mut state = TestEngineStateBuilder::new().build();

    let task = FinalizeTask::new(Arc::new(client), Arc::clone(&cfg), 0);
    task.execute(&mut state).await.expect("FinalizeTask should succeed");

    assert_eq!(
        state.sync_state.finalized_head().block_info.hash,
        hash,
        "finalized_head hash must equal the genesis block hash after finalization"
    );
}
