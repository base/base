//! Tests for [`FinalizeTask::execute`].

use std::sync::Arc;

use alloy_eips::BlockId;
use alloy_primitives::{B256, b256};
use alloy_rpc_types_engine::{ForkchoiceUpdated, PayloadStatus, PayloadStatusEnum};
use base_alloy_network::Base;
use base_consensus_genesis::{ChainGenesis, RollupConfig};
use alloy_eips::BlockNumHash;

use crate::{
    EngineTaskExt, FinalizeTask, FinalizeTaskError,
    test_utils::{TestEngineStateBuilder, test_engine_client_builder},
};

/// The OP Sepolia genesis block hash, used to construct a valid genesis block in tests.
const OP_SEPOLIA_GENESIS_HASH: B256 =
    b256!("102de6ffb001480cc9b8b548fd05c34cd4f46ae4aa91759393db90ea0409887d");

/// Minimal OP Sepolia genesis block as an RPC JSON response. Block number is 0.
const OP_SEPOLIA_GENESIS_JSON: &str = "{\"hash\":\"0x102de6ffb001480cc9b8b548fd05c34cd4f46ae4aa91759393db90ea0409887d\",\"parentHash\":\"0x0000000000000000000000000000000000000000000000000000000000000000\",\"sha3Uncles\":\"0x1dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0a142fd40d49347\",\"miner\":\"0x4200000000000000000000000000000000000011\",\"stateRoot\":\"0x06787a17a3ed87c339a39dbbeeb311578a0c83ed29daa2db95da62b28efce8a9\",\"transactionsRoot\":\"0x56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421\",\"receiptsRoot\":\"0x56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421\",\"logsBloom\":\"0x00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000\",\"difficulty\":\"0x0\",\"number\":\"0x0\",\"gasLimit\":\"0x1c9c380\",\"gasUsed\":\"0x0\",\"timestamp\":\"0x64d6dbac\",\"extraData\":\"0x424544524f434b\",\"mixHash\":\"0x0000000000000000000000000000000000000000000000000000000000000000\",\"nonce\":\"0x0000000000000000\",\"baseFeePerGas\":\"0x3b9aca00\",\"size\":\"0x209\",\"uncles\":[],\"transactions\":[]}";

fn valid_fcu() -> ForkchoiceUpdated {
    ForkchoiceUpdated {
        payload_status: PayloadStatus {
            status: PayloadStatusEnum::Valid,
            latest_valid_hash: Some(OP_SEPOLIA_GENESIS_HASH),
        },
        payload_id: None,
    }
}

/// A [`RollupConfig`] whose genesis L2 block matches the OP Sepolia genesis.
fn genesis_rollup_config() -> Arc<RollupConfig> {
    let mut cfg = RollupConfig::default();
    cfg.genesis = ChainGenesis {
        l2: BlockNumHash { number: 0, hash: OP_SEPOLIA_GENESIS_HASH },
        ..Default::default()
    };
    Arc::new(cfg)
}

/// Deserialize the OP Sepolia genesis block into the Base RPC block type.
fn genesis_rpc_block() -> alloy_rpc_types_eth::Block<<Base as alloy_network::Network>::TransactionResponse> {
    serde_json::from_str(OP_SEPOLIA_GENESIS_JSON).expect("valid genesis JSON")
}

#[tokio::test]
async fn block_not_safe_returns_error() {
    // safe_head = 5, block_number = 10 → BlockNotSafe
    let client = test_engine_client_builder().build();
    let state_block = crate::test_utils::test_block_info(5);
    let mut state =
        TestEngineStateBuilder::new().with_safe_head(state_block).with_unsafe_head(state_block).build();

    let task = FinalizeTask::new(Arc::new(client), Arc::new(RollupConfig::default()), 10);
    let result = task.execute(&mut state).await;

    assert!(matches!(result, Err(FinalizeTaskError::BlockNotSafe)), "expected BlockNotSafe, got {result:?}");
}

#[tokio::test]
async fn block_not_found_returns_error() {
    // safe_head = 10, block_number = 7, no block registered in mock → BlockNotFound
    let client = test_engine_client_builder().build();
    let state_block = crate::test_utils::test_block_info(10);
    let mut state =
        TestEngineStateBuilder::new().with_safe_head(state_block).with_unsafe_head(state_block).build();

    let task = FinalizeTask::new(Arc::new(client), Arc::new(RollupConfig::default()), 7);
    let result = task.execute(&mut state).await;

    assert!(
        matches!(result, Err(FinalizeTaskError::BlockNotFound(7))),
        "expected BlockNotFound(7), got {result:?}"
    );
}

#[tokio::test]
async fn from_block_error_on_genesis_hash_mismatch() {
    // Provide the OP Sepolia genesis block but configure genesis.l2.hash to a wrong value.
    // from_block_and_genesis must return InvalidGenesisHash → FinalizeTaskError::FromBlock.
    let wrong_hash = B256::ZERO;
    let mut cfg = RollupConfig::default();
    cfg.genesis = ChainGenesis {
        l2: BlockNumHash { number: 0, hash: wrong_hash },
        ..Default::default()
    };

    let client = test_engine_client_builder()
        .with_config(Arc::new(cfg.clone()))
        .with_l2_block(BlockId::Number(alloy_eips::BlockNumberOrTag::Number(0).into()), genesis_rpc_block())
        .build();

    let state_block = crate::test_utils::test_block_info(0);
    let mut state = TestEngineStateBuilder::new()
        .with_safe_head(state_block)
        .with_unsafe_head(state_block)
        .build();

    let task = FinalizeTask::new(Arc::new(client), Arc::new(cfg), 0);
    let result = task.execute(&mut state).await;

    assert!(
        matches!(result, Err(FinalizeTaskError::FromBlock(_))),
        "expected FromBlock error on genesis hash mismatch, got {result:?}"
    );
}

#[tokio::test]
async fn fcu_failure_propagates_as_forkchoice_update_failed() {
    // Provide a valid genesis block but do NOT configure a FCU response.
    // SynchronizeTask will fail with a transport error → ForkchoiceUpdateFailed.
    let cfg = genesis_rollup_config();
    let client = test_engine_client_builder()
        .with_config(Arc::clone(&cfg))
        .with_l2_block(BlockId::Number(alloy_eips::BlockNumberOrTag::Number(0).into()), genesis_rpc_block())
        // fork_choice_updated_v3 is intentionally NOT configured.
        .build();

    let state_block = crate::test_utils::test_block_info(0);
    let mut state = TestEngineStateBuilder::new()
        .with_safe_head(state_block)
        .with_unsafe_head(state_block)
        .build();

    let task = FinalizeTask::new(Arc::new(client), Arc::clone(&cfg), 0);
    let result = task.execute(&mut state).await;

    assert!(
        matches!(result, Err(FinalizeTaskError::ForkchoiceUpdateFailed(_))),
        "expected ForkchoiceUpdateFailed, got {result:?}"
    );
}

#[tokio::test]
async fn success_updates_engine_state_finalized_head() {
    // Full happy path: fetch genesis block, pass from_block_and_genesis, dispatch FCU.
    // The state's finalized_head starts at B256::ZERO; after the task it should be the
    // OP Sepolia genesis hash, confirming the FCU was called with the correct block.
    let cfg = genesis_rollup_config();
    let client = test_engine_client_builder()
        .with_config(Arc::clone(&cfg))
        .with_l2_block(BlockId::Number(alloy_eips::BlockNumberOrTag::Number(0).into()), genesis_rpc_block())
        .with_fork_choice_updated_v3_response(valid_fcu())
        .build();

    // Default TestEngineStateBuilder starts finalized_head with hash = B256::ZERO.
    // The genesis block we provide has hash = OP_SEPOLIA_GENESIS_HASH, so the state
    // differs → SynchronizeTask will call FCU and update the state.
    let mut state = TestEngineStateBuilder::new().build();

    let task = FinalizeTask::new(Arc::new(client), Arc::clone(&cfg), 0);
    task.execute(&mut state).await.expect("FinalizeTask should succeed");

    assert_eq!(
        state.sync_state.finalized_head().block_info.hash,
        OP_SEPOLIA_GENESIS_HASH,
        "finalized_head hash should match the genesis block after successful finalization"
    );
}
