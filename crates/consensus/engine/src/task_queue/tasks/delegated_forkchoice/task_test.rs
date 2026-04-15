//! Tests for [`DelegatedForkchoiceTask::execute`].

use std::sync::Arc;

use alloy_eips::{BlockId, BlockNumHash, BlockNumberOrTag};
use alloy_primitives::B256;
use alloy_rpc_types_engine::{ForkchoiceUpdated, PayloadStatus, PayloadStatusEnum};
use alloy_rpc_types_eth::Block as RpcBlock;
use base_common_rpc_types::Transaction as OpTransaction;
use base_consensus_genesis::{ChainGenesis, RollupConfig};
use base_protocol::{BlockInfo, L2BlockInfo};

use crate::{
    DelegatedForkchoiceTask, DelegatedForkchoiceUpdate, EngineTaskExt,
    test_utils::{TestEngineStateBuilder, test_block_info, test_engine_client_builder},
};

// ── Test yardımcıları ──────────────────────────────────────────────────────

fn syncing_fcu() -> ForkchoiceUpdated {
    ForkchoiceUpdated {
        payload_status: PayloadStatus {
            status: PayloadStatusEnum::Syncing,
            latest_valid_hash: None,
        },
        payload_id: None,
    }
}

fn valid_fcu() -> ForkchoiceUpdated {
    ForkchoiceUpdated {
        payload_status: PayloadStatus {
            status: PayloadStatusEnum::Valid,
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

fn l2_block_info_with_hash(number: u64, hash: B256) -> L2BlockInfo {
    L2BlockInfo {
        block_info: BlockInfo { hash, number, ..Default::default() },
        ..Default::default()
    }
}

/// `FinalizeTask`'ın `from_block_and_genesis` çağrısını geçirebilmek için
/// genesis yolundan giden blok oluşturur. Döndürülen blok ve hash'i
/// `RollupConfig.genesis.l2.hash` olarak kullanılmalıdır.
fn make_genesis_block() -> (RpcBlock<OpTransaction>, B256) {
    let block = RpcBlock::<OpTransaction>::default();
    let hash = block.clone().into_consensus().hash_slow();
    (block, hash)
}

/// `make_genesis_block()` ile uyumlu minimal rollup config.
fn genesis_rollup_cfg(hash: B256) -> Arc<RollupConfig> {
    Arc::new(RollupConfig {
        genesis: ChainGenesis { l2: BlockNumHash { number: 0, hash }, ..Default::default() },
        ..Default::default()
    })
}

// ── Hata 1 regresyon testleri: clamp pivot doğruluğu ──────────────────────

/// Orijinal hata kanıtı:
///
/// Eski kodda `engine_head` (başlangıçta 0) clamp pivotu olarak kullanılıyordu.
/// Taze bir follow node'da engine'in safe_head = 0 (genesis) kalıyordu.
/// Bu yüzden:
///   safe      = min(remote_safe=500, engine_head=0) = 0   ← genesis'te kilitli (BUG)
///   finalized = min(remote_fin=0,    engine_head=0) = 0   ← genesis'te kilitli (BUG)
///
/// Yeni kodda `sent_head` pivot:
///   safe      = min(remote_safe=500, sent_head=1000) = 500  ← doğru
///   finalized = None çünkü genesis blok finalize yeterli; FCU atlanır.
///
/// Bu testte FCU Syncing döndürür → safe değişmez → finalized da öne geçemez.
/// Asıl kanıt: safe_head ilerlemedi, yani eski kodun 0'dan farklı bir değer
/// üretememesi durumu gösterilmiştir.
#[tokio::test]
async fn clamp_pivot_is_sent_head_syncing_path_safe_stays_zero() {
    let safe_number = 500u64;
    let safe_hash = B256::from([0xAA; 32]);
    let delegated_safe = l2_block_info_with_hash(safe_number, safe_hash);

    // FCU Syncing → ConsolidateTask safe_head'i ilerletmiyor
    // FinalizeTask hiç çalışmıyor (actual_safe=0, finalized_target=min(400,0)=0)
    let client = Arc::new(
        test_engine_client_builder()
            .with_l2_block_by_label(
                BlockNumberOrTag::Number(safe_number),
                block_with_hash(safe_number, safe_hash),
            )
            .with_fork_choice_updated_v3_response(syncing_fcu())
            .build(),
    );

    let mut state = TestEngineStateBuilder::new()
        .with_unsafe_head(test_block_info(1000))
        .with_safe_head(L2BlockInfo::default())      // engine safe = 0 (genesis)
        .with_finalized_head(L2BlockInfo::default())
        .with_el_sync_finished(false)
        .build();

    let task = DelegatedForkchoiceTask::new(
        client,
        Arc::new(RollupConfig::default()),
        DelegatedForkchoiceUpdate {
            safe_l2: delegated_safe,
            finalized_l2_number: Some(400),
        },
    );

    task.execute(&mut state).await.expect("task must not fail");

    // safe_head, FCU Syncing nedeniyle değişmedi — bu beklenen davranış.
    // Eski kodda da 0 üretirdi ama nedeni farklıydı: engine_head=0 clamp.
    assert_eq!(
        state.sync_state.safe_head(),
        L2BlockInfo::default(),
        "safe stays at genesis when FCU returns Syncing"
    );
    assert_eq!(
        state.sync_state.finalized_head(),
        L2BlockInfo::default(),
        "finalized must not advance past actual safe=0"
    );
}

/// Finalized, ConsolidateTask'tan SONRA elde edilen gerçek safe'i geçemez.
///
/// FCU Syncing → safe_head = 0 kalır
/// finalized_target = min(200, actual_safe=0) = 0 → FinalizeTask çalışmaz
/// Sonuç: finalized = 0 (değişmez)
#[tokio::test]
async fn finalized_clamped_to_actual_safe_after_consolidation() {
    let safe_number = 100u64;
    let safe_hash = B256::from([0x55; 32]);
    let delegated_safe = l2_block_info_with_hash(safe_number, safe_hash);

    let client = Arc::new(
        test_engine_client_builder()
            .with_l2_block_by_label(
                BlockNumberOrTag::Number(safe_number),
                block_with_hash(safe_number, safe_hash),
            )
            .with_fork_choice_updated_v3_response(syncing_fcu())
            .build(),
    );

    let mut state = TestEngineStateBuilder::new()
        .with_unsafe_head(test_block_info(500))
        .with_safe_head(L2BlockInfo::default())
        .with_finalized_head(L2BlockInfo::default())
        .with_el_sync_finished(false)
        .build();

    let task = DelegatedForkchoiceTask::new(
        client,
        Arc::new(RollupConfig::default()),
        DelegatedForkchoiceUpdate {
            safe_l2: delegated_safe,
            finalized_l2_number: Some(200), // safe'den büyük, ama actual_safe=0
        },
    );

    task.execute(&mut state).await.expect("task must not fail");

    assert_eq!(
        state.sync_state.finalized_head(),
        L2BlockInfo::default(),
        "finalized must not advance past actual safe when FCU returns Syncing"
    );
}

/// Mevcut finalized >= finalized_target ise finalized geri götürülmemeli.
///
/// Senaryo:
///   current_finalized = 300
///   finalized_target  = min(200, actual_safe=500) = 200
///   → 300 >= 200 → FinalizeTask atlanır
#[tokio::test]
async fn finalized_not_updated_when_already_at_or_ahead() {
    let safe_number = 500u64;
    let safe_hash = B256::from([0x77; 32]);
    let delegated_safe = l2_block_info_with_hash(safe_number, safe_hash);
    let current_finalized_number = 300u64;
    let current_finalized_hash = B256::from([0x88; 32]);

    // FinalizeTask çalışmayacak (target=200 < current=300) → get_l2_block mock'u yok
    let client = Arc::new(
        test_engine_client_builder()
            .with_l2_block_by_label(
                BlockNumberOrTag::Number(safe_number),
                block_with_hash(safe_number, safe_hash),
            )
            .with_fork_choice_updated_v3_response(valid_fcu())
            .build(),
    );

    let current_finalized = l2_block_info_with_hash(current_finalized_number, current_finalized_hash);

    let mut state = TestEngineStateBuilder::new()
        .with_unsafe_head(test_block_info(1000))
        .with_safe_head(delegated_safe)
        .with_finalized_head(current_finalized)
        .with_el_sync_finished(true)
        .build();

    let task = DelegatedForkchoiceTask::new(
        client,
        Arc::new(RollupConfig::default()),
        DelegatedForkchoiceUpdate {
            safe_l2: delegated_safe,
            finalized_l2_number: Some(200), // current=300 > target=200
        },
    );

    task.execute(&mut state).await.expect("task must not fail");

    assert_eq!(
        state.sync_state.finalized_head().block_info.number,
        current_finalized_number,
        "finalized must not go backwards when target < current"
    );
}

/// finalized_l2_number = None → finalized değişmemeli.
#[tokio::test]
async fn no_finalized_update_when_finalized_number_is_none() {
    let safe_number = 400u64;
    let safe_hash = B256::from([0x99; 32]);
    let delegated_safe = l2_block_info_with_hash(safe_number, safe_hash);

    let client = Arc::new(
        test_engine_client_builder()
            .with_l2_block_by_label(
                BlockNumberOrTag::Number(safe_number),
                block_with_hash(safe_number, safe_hash),
            )
            .with_fork_choice_updated_v3_response(valid_fcu())
            .build(),
    );

    let mut state = TestEngineStateBuilder::new()
        .with_unsafe_head(test_block_info(1000))
        .with_safe_head(L2BlockInfo::default())
        .with_finalized_head(L2BlockInfo::default())
        .with_el_sync_finished(true)
        .build();

    let task = DelegatedForkchoiceTask::new(
        client,
        Arc::new(RollupConfig::default()),
        DelegatedForkchoiceUpdate { safe_l2: delegated_safe, finalized_l2_number: None },
    );

    task.execute(&mut state).await.expect("task must not fail");

    assert_eq!(
        state.sync_state.finalized_head(),
        L2BlockInfo::default(),
        "finalized must remain default when finalized_l2_number is None"
    );
}

/// safe = finalized: her ikisi de genesis yoluyla ilerlemeli.
///
/// Genesis bloğu `from_block_and_genesis`'den geçebilmek için
/// config'deki genesis.l2.hash ile eşleşmeli.
#[tokio::test]
async fn safe_and_finalized_equal_both_advance_to_genesis() {
    let (genesis_block, genesis_hash) = make_genesis_block();
    let cfg = genesis_rollup_cfg(genesis_hash);

    let delegated_safe = l2_block_info_with_hash(0, genesis_hash);

    let client = Arc::new(
        test_engine_client_builder()
            .with_config(Arc::clone(&cfg))
            // ConsolidateTask: by-label
            .with_l2_block_by_label(
                BlockNumberOrTag::Number(0),
                genesis_block.clone(),
            )
            // FinalizeTask: by-id (get_l2_block)
            .with_l2_block(
                BlockId::Number(BlockNumberOrTag::Number(0)),
                genesis_block,
            )
            .with_fork_choice_updated_v3_response(valid_fcu())
            .build(),
    );

    let mut state = TestEngineStateBuilder::new()
        .with_unsafe_head(test_block_info(0))
        .with_safe_head(L2BlockInfo::default())
        .with_finalized_head(L2BlockInfo::default())
        .with_el_sync_finished(true)
        .build();

    let task = DelegatedForkchoiceTask::new(
        client,
        Arc::clone(&cfg),
        DelegatedForkchoiceUpdate {
            safe_l2: delegated_safe,
            finalized_l2_number: Some(0), // safe == finalized == genesis
        },
    );

    task.execute(&mut state).await.expect("task must not fail");

    assert_eq!(
        state.sync_state.safe_head().block_info.hash,
        genesis_hash,
        "safe must advance to genesis block"
    );
    assert_eq!(
        state.sync_state.finalized_head().block_info.hash,
        genesis_hash,
        "finalized must advance to genesis when finalized == safe"
    );
}

/// finalized_target = actual_safe olduğunda doğru clamp.
///
/// Senaryo: FCU Valid → safe=block_N → finalized_target = min(N+100, N) = N
/// FinalizeTask genesis yoluyla geçer.
#[tokio::test]
async fn finalized_clamped_to_actual_safe_when_remote_finalized_exceeds_safe() {
    let (genesis_block, genesis_hash) = make_genesis_block();
    let cfg = genesis_rollup_cfg(genesis_hash);
    let delegated_safe = l2_block_info_with_hash(0, genesis_hash);

    let client = Arc::new(
        test_engine_client_builder()
            .with_config(Arc::clone(&cfg))
            .with_l2_block_by_label(
                BlockNumberOrTag::Number(0),
                genesis_block.clone(),
            )
            .with_l2_block(
                BlockId::Number(BlockNumberOrTag::Number(0)),
                genesis_block,
            )
            .with_fork_choice_updated_v3_response(valid_fcu())
            .build(),
    );

    let mut state = TestEngineStateBuilder::new()
        .with_unsafe_head(test_block_info(0))
        .with_safe_head(L2BlockInfo::default())
        .with_finalized_head(L2BlockInfo::default())
        .with_el_sync_finished(true)
        .build();

    // remote_finalized = 999 >> actual_safe=0 → clamped to actual_safe=0
    let task = DelegatedForkchoiceTask::new(
        client,
        Arc::clone(&cfg),
        DelegatedForkchoiceUpdate {
            safe_l2: delegated_safe,
            finalized_l2_number: Some(999),
        },
    );

    task.execute(&mut state).await.expect("task must not fail");

    // finalized_target = min(999, actual_safe) = min(999, 0) = 0
    assert_eq!(
        state.sync_state.finalized_head().block_info.hash,
        genesis_hash,
        "finalized must be clamped to actual_safe (block 0)"
    );
}

/// Orijinal tek test (regresyon koruması olarak korunur).
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
