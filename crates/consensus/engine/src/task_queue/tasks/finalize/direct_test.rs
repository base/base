//! Tests for finalization direct execution.

use std::sync::Arc;

use alloy_eips::{BlockId, BlockNumHash, BlockNumberOrTag};
use alloy_primitives::{B256, b256};
use alloy_rpc_types_engine::{ForkchoiceUpdated, PayloadStatus, PayloadStatusEnum};
use alloy_rpc_types_eth::Block as RpcBlock;
use base_common_genesis::{ChainGenesis, RollupConfig};
use base_common_rpc_types::Transaction as BaseTransaction;
use rstest::rstest;

use crate::{
    Engine, FinalizeTaskError,
    test_utils::{TestEngineStateBuilder, test_block_info, test_engine_client_builder},
};

/// The genesis block hash for Base Sepolia (block 0).
const BASE_SEPOLIA_GENESIS_HASH: B256 =
    b256!("0dcc9e089e30b90ddfc55be9a37dd15bc551aeee999d2e2b51414c54eaf934e4");

/// The genesis block hash for Base Mainnet (block 0).
const BASE_MAINNET_GENESIS_HASH: B256 =
    b256!("f712aa9241cc24369b143cf6dce85f0902a9731e70d66818a3a5845b296c73dd");

/// Construct a minimal default genesis block for testing finalization.
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

#[derive(Debug)]
enum ExpectedFinalizeError {
    BlockNotSafe,
    BlockNotFound(u64),
    FromBlock,
    ForkchoiceUpdateFailed,
}

impl ExpectedFinalizeError {
    fn matches(&self, result: &Result<(), FinalizeTaskError>) -> bool {
        match (self, result) {
            (Self::BlockNotFound(expected), Err(FinalizeTaskError::BlockNotFound(actual))) => {
                expected == actual
            }
            (Self::BlockNotSafe, Err(FinalizeTaskError::BlockNotSafe))
            | (Self::FromBlock, Err(FinalizeTaskError::FromBlock(_)))
            | (Self::ForkchoiceUpdateFailed, Err(FinalizeTaskError::ForkchoiceUpdateFailed(_))) => {
                true
            }
            _ => false,
        }
    }
}

#[derive(Debug)]
enum GenesisFinalizeFailure {
    HashMismatch,
    MissingFcu,
}

#[rstest]
#[case::block_not_safe(5, 10, ExpectedFinalizeError::BlockNotSafe)]
#[case::block_not_found(10, 7, ExpectedFinalizeError::BlockNotFound(7))]
#[tokio::test]
async fn direct_finalize_block_validation_errors(
    #[case] safe_head: u64,
    #[case] block_number: u64,
    #[case] expected: ExpectedFinalizeError,
) {
    let client = test_engine_client_builder().build();
    let head = test_block_info(safe_head);
    let mut state =
        TestEngineStateBuilder::new().with_safe_head(head).with_unsafe_head(head).build();

    let result = Engine::finalize_with_state(
        &mut state,
        Arc::new(client),
        Arc::new(RollupConfig::default()),
        block_number,
    )
    .await;

    assert!(expected.matches(&result), "expected {expected:?}, got {result:?}");
}

#[rstest]
#[case::genesis_hash_mismatch(
    GenesisFinalizeFailure::HashMismatch,
    ExpectedFinalizeError::FromBlock
)]
#[case::missing_fcu(
    GenesisFinalizeFailure::MissingFcu,
    ExpectedFinalizeError::ForkchoiceUpdateFailed
)]
#[tokio::test]
async fn direct_finalize_genesis_errors(
    #[case] failure: GenesisFinalizeFailure,
    #[case] expected: ExpectedFinalizeError,
) {
    let (block, hash) = make_genesis_block();
    let cfg = match failure {
        GenesisFinalizeFailure::HashMismatch => genesis_rollup_cfg(BASE_SEPOLIA_GENESIS_HASH),
        GenesisFinalizeFailure::MissingFcu => genesis_rollup_cfg(hash),
    };

    let client = test_engine_client_builder()
        .with_config(Arc::clone(&cfg))
        .with_l2_block(BlockId::Number(BlockNumberOrTag::Number(0)), block)
        .build();
    let head = test_block_info(0);
    let mut state =
        TestEngineStateBuilder::new().with_safe_head(head).with_unsafe_head(head).build();

    let result = Engine::finalize_with_state(&mut state, Arc::new(client), cfg, 0).await;

    assert!(expected.matches(&result), "expected {expected:?}, got {result:?}");
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

    Engine::finalize_with_state(&mut state, Arc::new(client), Arc::clone(&cfg), 0)
        .await
        .expect("finalization should succeed");

    assert_eq!(
        state.sync_state.finalized_head().block_info.hash,
        hash,
        "finalized_head hash must equal the genesis block hash after finalization"
    );
}
