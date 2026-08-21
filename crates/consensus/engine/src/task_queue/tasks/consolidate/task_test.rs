//! Tests for `ConsolidateTask::execute`

use std::{sync::Arc, time::Duration};

use alloy_consensus::transaction::Recovered;
use alloy_eips::{BlockNumberOrTag, Encodable2718};
use alloy_primitives::{Address, B256, Bytes, FixedBytes, b256};
use alloy_rpc_types_engine::{ForkchoiceUpdated, PayloadId, PayloadStatus, PayloadStatusEnum};
use alloy_rpc_types_eth::{Block as RpcBlock, BlockTransactions};
use base_common_consensus::{BaseTxEnvelope, TxDeposit};
use base_common_genesis::RollupConfig;
use base_common_rpc_types::Transaction as BaseTransaction;
use base_protocol::{AttributesWithParent, BlockInfo, L1BlockInfoBedrock, L2BlockInfo};
use tokio::{sync::watch, time::timeout};

use crate::{
    AttributesMatch, AttributesMismatch, ConsolidateTask, Engine, EngineTask, EngineTaskError,
    EngineTaskErrorSeverity, EngineTaskExt, SynchronizeTask,
    state::EngineSyncStateUpdate,
    task_queue::tasks::consolidate::task::ConsolidateInput,
    test_utils::{TestAttributesBuilder, TestEngineStateBuilder, test_engine_client_builder},
};

fn l1_info_deposit_tx() -> BaseTxEnvelope {
    BaseTxEnvelope::from(TxDeposit {
        input: L1BlockInfoBedrock::default().encode_calldata(),
        ..Default::default()
    })
}

fn encoded_l1_info_deposit_tx() -> Bytes {
    let mut tx = Vec::new();
    l1_info_deposit_tx().encode_2718(&mut tx);
    tx.into()
}

fn rpc_transaction(tx: BaseTxEnvelope, block_number: u64) -> BaseTransaction {
    BaseTransaction {
        inner: alloy_rpc_types_eth::Transaction {
            inner: Recovered::new_unchecked(tx, Address::ZERO),
            block_hash: None,
            block_number: Some(block_number),
            block_timestamp: None,
            effective_gas_price: Some(0),
            transaction_index: Some(0),
        },
        block_timestamp_ms: None,
        deposit_nonce: None,
        deposit_receipt_version: None,
    }
}

fn l2_block_info(number: u64, hash: B256, parent_hash: B256, timestamp: u64) -> L2BlockInfo {
    L2BlockInfo {
        block_info: BlockInfo { number, hash, parent_hash, timestamp },
        l1_origin: Default::default(),
        seq_num: 0,
    }
}

fn matching_rpc_block(
    block_info: L2BlockInfo,
    attributes: &AttributesWithParent,
) -> RpcBlock<BaseTransaction> {
    let mut block = RpcBlock::<BaseTransaction>::default();
    block.header.hash = block_info.block_info.hash;
    block.header.inner.number = block_info.block_info.number;
    block.header.inner.parent_hash = attributes.parent.block_info.hash;
    block.header.inner.timestamp = attributes.attributes().payload_attributes.timestamp;
    block.header.inner.mix_hash = attributes.attributes().payload_attributes.prev_randao;
    block.header.inner.gas_limit = attributes.attributes().gas_limit.unwrap_or_default();
    block.header.inner.parent_beacon_block_root =
        attributes.attributes().payload_attributes.parent_beacon_block_root;
    block.header.inner.beneficiary =
        attributes.attributes().payload_attributes.suggested_fee_recipient;
    block.transactions = BlockTransactions::Full(vec![rpc_transaction(
        l1_info_deposit_tx(),
        block_info.block_info.number,
    )]);
    block
}

fn block_info_from_rpc_block(block: RpcBlock<BaseTransaction>, cfg: &RollupConfig) -> L2BlockInfo {
    L2BlockInfo::from_block_and_genesis(
        &block.into_consensus().map_transactions(|tx| tx.inner.inner.into_inner()),
        &cfg.genesis,
    )
    .expect("test block must decode as L2BlockInfo")
}

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
        payload_status: PayloadStatus { status: PayloadStatusEnum::Valid, latest_valid_hash: None },
        payload_id: None,
    }
}

fn valid_build_fcu() -> ForkchoiceUpdated {
    ForkchoiceUpdated {
        payload_status: PayloadStatus { status: PayloadStatusEnum::Valid, latest_valid_hash: None },
        payload_id: Some(PayloadId::new([9u8; 8])),
    }
}

/// Verifies that consolidation does NOT fatally error when safe head is behind
/// the unsafe head and the derived attributes don't match the existing block.
///
/// Previously, `SealTask` compared `state.sync_state.unsafe_head()` (the chain
/// tip, e.g. block 76) against `attributes.parent` (the safe head, e.g. block 34)
/// and returned `UnsafeHeadChangedSinceBuild` with Critical severity, crashing the
/// engine. Op-node has no such check; the build step already FCU'd the EL to the
/// correct parent, so the comparison is invalid.
///
/// After the fix the reconcile path proceeds to `seal_and_canonicalize_block`
/// directly, matching the reference node's behaviour.
///
/// This test FAILS on unfixed main and PASSES after the fix lands.
#[tokio::test]
async fn consolidate_does_not_crash_when_safe_behind_unsafe_and_attributes_mismatch() {
    let safe_head = crate::test_utils::test_block_info(34);
    let unsafe_head = crate::test_utils::test_block_info(76);

    // Attributes produced by derivation: parent = safe_head (block 34) → block 35.
    let attributes =
        TestAttributesBuilder::new().with_parent(safe_head).with_timestamp(2000).build();

    // Engine state: safe at 34, unsafe at 76.
    let mut state = TestEngineStateBuilder::new()
        .with_unsafe_head(unsafe_head)
        .with_safe_head(safe_head)
        .with_finalized_head(safe_head)
        .build();

    // Build a block at height 35 that does NOT match the attributes.
    // The key mismatch: parent_hash differs from attributes.parent.block_info.hash.
    // This makes `is_consistent_with_block` return false → triggers reconcile path.
    let mut mismatched_block = RpcBlock::<BaseTransaction>::default();
    mismatched_block.header.inner.number = 35;
    mismatched_block.header.inner.timestamp = 2000;
    mismatched_block.header.inner.parent_hash =
        b256!("deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef");

    // Mock client: return the mismatched block at number 35, and a Valid FCU
    // with a payload_id needed by the build step inside the reconcile path.
    let valid_fcu = ForkchoiceUpdated {
        payload_status: PayloadStatus {
            status: PayloadStatusEnum::Valid,
            latest_valid_hash: Some(FixedBytes([2u8; 32])),
        },
        payload_id: Some(PayloadId::new([1u8; 8])),
    };
    let client = Arc::new(
        test_engine_client_builder()
            .with_l2_block_by_label(BlockNumberOrTag::Number(35), mismatched_block)
            .with_fork_choice_updated_v2_response(valid_fcu.clone())
            .with_fork_choice_updated_v3_response(valid_fcu)
            .build(),
    );

    let task = ConsolidateTask::new(
        client,
        Arc::new(RollupConfig::default()),
        ConsolidateInput::from(attributes),
    );

    // Execute — previously this returned Critical UnsafeHeadChangedSinceBuild.
    // Now it proceeds to seal_and_canonicalize_block (which will fail for other
    // reasons in a mock environment, but crucially NOT with the stale-unsafe-head
    // check that caused the crash loop).
    let result = task.execute(&mut state).await;

    // The task may still error (e.g. GetPayload fails in the mock) but it must
    // NOT be the stale-unsafe-head error that caused the crash loop.
    // The Display string for SealTaskError::UnsafeHeadChangedSinceBuild is
    // "Unsafe head changed between build and seal".
    if let Err(ref err) = result {
        let err_msg = format!("{err}");
        assert!(
            !err_msg.contains("Unsafe head changed between build and seal"),
            "must not fail with UnsafeHeadChangedSinceBuild: {err}"
        );
    }
}

#[tokio::test]
async fn consolidate_reconciles_unadvanced_unsafe_before_non_span_safe_attributes() {
    let cfg = Arc::new(RollupConfig::default());
    let pinned_head = l2_block_info(
        10,
        b256!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
        b256!("9999999999999999999999999999999999999999999999999999999999999999"),
        20,
    );
    let pending_unsafe = l2_block_info(
        40,
        b256!("dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd"),
        b256!("cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"),
        80,
    );
    let safe_child = l2_block_info(
        11,
        b256!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
        pinned_head.block_info.hash,
        22,
    );
    let attributes = TestAttributesBuilder::new()
        .with_parent(pinned_head)
        .with_timestamp(safe_child.block_info.timestamp)
        .with_transactions(vec![encoded_l1_info_deposit_tx()])
        .with_is_last_in_span(false)
        .build();

    let client = Arc::new(
        test_engine_client_builder()
            .with_fork_choice_updated_v2_response(valid_build_fcu())
            .with_fork_choice_updated_v3_response(syncing_fcu())
            .build(),
    );
    let mut state = TestEngineStateBuilder::new()
        .with_unsafe_head(pinned_head)
        .with_safe_head(pinned_head)
        .with_finalized_head(pinned_head)
        .with_el_sync_finished(true)
        .build();

    // A live unsafe target was accepted by newPayload, but its bare FCU returned SYNCING while
    // reth backfilled. Consensus therefore remains pinned at the old unsafe head.
    SynchronizeTask::new(
        Arc::clone(&client),
        Arc::clone(&cfg),
        EngineSyncStateUpdate { unsafe_head: Some(pending_unsafe), ..Default::default() },
    )
    .execute(&mut state)
    .await
    .expect("SYNCING unsafe FCU should be non-fatal");
    assert_eq!(state.sync_state.unsafe_head(), pinned_head);

    // EL sync has now caught up enough to serve the next safe block that derivation is about to
    // confirm.
    client.set_fork_choice_updated_v3_response(valid_fcu()).await;
    let safe_child_block = matching_rpc_block(safe_child, &attributes);
    let expected_safe_child = block_info_from_rpc_block(safe_child_block.clone(), &cfg);
    client
        .set_l2_block_by_label(
            BlockNumberOrTag::Number(safe_child.block_info.number),
            safe_child_block,
        )
        .await;

    ConsolidateTask::new(Arc::clone(&client), Arc::clone(&cfg), ConsolidateInput::from(attributes))
        .execute(&mut state)
        .await
        .expect("safe derivation should reconcile the available unsafe block instead of building");

    assert!(
        state.sync_state.unsafe_head().block_info.number >= expected_safe_child.block_info.number,
        "unsafe head must advance before safe derivation advances"
    );
    assert_eq!(state.sync_state.local_safe_head(), expected_safe_child);
    assert_eq!(state.sync_state.safe_head(), expected_safe_child);

    let storage = client.storage();
    let requests = storage.read().await;
    assert!(
        requests
            .fork_choice_updated_v2_requests
            .iter()
            .chain(requests.fork_choice_updated_v3_requests.iter())
            .all(|(_, has_attrs)| !has_attrs),
        "safe reconciliation must not start a stale FCU-with-attributes build"
    );
}

#[tokio::test]
async fn consolidate_syncing_yields_until_a_later_drain() {
    let cfg = Arc::new(RollupConfig::default());
    let pinned_head = l2_block_info(
        10,
        b256!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
        b256!("9999999999999999999999999999999999999999999999999999999999999999"),
        20,
    );
    let safe_child = l2_block_info(
        11,
        b256!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
        pinned_head.block_info.hash,
        22,
    );
    let attributes = TestAttributesBuilder::new()
        .with_parent(pinned_head)
        .with_timestamp(safe_child.block_info.timestamp)
        .with_transactions(vec![encoded_l1_info_deposit_tx()])
        .build();
    let safe_child_block = matching_rpc_block(safe_child, &attributes);
    let expected_safe_child = block_info_from_rpc_block(safe_child_block.clone(), &cfg);
    let client = Arc::new(
        test_engine_client_builder()
            .with_l2_block_by_label(
                BlockNumberOrTag::Number(safe_child.block_info.number),
                safe_child_block,
            )
            .with_fork_choice_updated_v3_response(syncing_fcu())
            .build(),
    );
    let initial_state = TestEngineStateBuilder::new()
        .with_unsafe_head(pinned_head)
        .with_safe_head(pinned_head)
        .with_finalized_head(pinned_head)
        .with_el_sync_finished(true)
        .build();
    let (state_tx, _) = watch::channel(initial_state);
    let (queue_tx, queue_rx) = watch::channel(0usize);
    let mut engine = Engine::new(initial_state, state_tx, queue_tx);
    engine.enqueue(EngineTask::Consolidate(Box::new(ConsolidateTask::new(
        Arc::clone(&client),
        Arc::clone(&cfg),
        ConsolidateInput::from(attributes),
    ))));

    let err = timeout(Duration::from_secs(1), engine.drain())
        .await
        .expect("a deferred consolidation failure must yield to the engine processor")
        .expect_err("SYNCING must keep the consolidation task pending");

    assert_eq!(err.severity(), EngineTaskErrorSeverity::Deferred);
    assert_eq!(*queue_rx.borrow(), 1, "deferred task must remain queued");
    assert_eq!(engine.state().sync_state.unsafe_head(), pinned_head);

    client.set_fork_choice_updated_v3_response(valid_fcu()).await;
    engine.drain().await.expect("a later drain should retry the pending consolidation");

    assert_eq!(*queue_rx.borrow(), 0);
    assert_eq!(engine.state().sync_state.unsafe_head(), expected_safe_child);
    assert_eq!(engine.state().sync_state.local_safe_head(), expected_safe_child);
    assert_eq!(engine.state().sync_state.safe_head(), expected_safe_child);
}

#[tokio::test]
async fn consolidate_reconciles_unadvanced_unsafe_before_last_span_safe_attributes() {
    let cfg = Arc::new(RollupConfig::default());
    let pinned_head = l2_block_info(
        10,
        b256!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
        b256!("9999999999999999999999999999999999999999999999999999999999999999"),
        20,
    );
    let pending_unsafe = l2_block_info(
        40,
        b256!("dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd"),
        b256!("cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"),
        80,
    );
    let safe_child = l2_block_info(
        11,
        b256!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
        pinned_head.block_info.hash,
        22,
    );
    let attributes = TestAttributesBuilder::new()
        .with_parent(pinned_head)
        .with_timestamp(safe_child.block_info.timestamp)
        .with_transactions(vec![encoded_l1_info_deposit_tx()])
        .with_is_last_in_span(true)
        .build();

    let client = Arc::new(
        test_engine_client_builder()
            .with_fork_choice_updated_v2_response(valid_build_fcu())
            .with_fork_choice_updated_v3_response(syncing_fcu())
            .build(),
    );
    let mut state = TestEngineStateBuilder::new()
        .with_unsafe_head(pinned_head)
        .with_safe_head(pinned_head)
        .with_finalized_head(pinned_head)
        .with_el_sync_finished(true)
        .build();

    SynchronizeTask::new(
        Arc::clone(&client),
        Arc::clone(&cfg),
        EngineSyncStateUpdate { unsafe_head: Some(pending_unsafe), ..Default::default() },
    )
    .execute(&mut state)
    .await
    .expect("SYNCING unsafe FCU should be non-fatal");
    assert_eq!(state.sync_state.unsafe_head(), pinned_head);

    client.set_fork_choice_updated_v3_response(valid_fcu()).await;
    let safe_child_block = matching_rpc_block(safe_child, &attributes);
    let expected_safe_child = block_info_from_rpc_block(safe_child_block.clone(), &cfg);
    client
        .set_l2_block_by_label(
            BlockNumberOrTag::Number(safe_child.block_info.number),
            safe_child_block,
        )
        .await;

    ConsolidateTask::new(Arc::clone(&client), Arc::clone(&cfg), ConsolidateInput::from(attributes))
        .execute(&mut state)
        .await
        .expect("span-ending safe derivation should use a bare FCU after unsafe reconciliation");

    assert!(
        state.sync_state.unsafe_head().block_info.number >= expected_safe_child.block_info.number,
        "unsafe head must advance before safe derivation advances"
    );
    assert_eq!(state.sync_state.local_safe_head(), expected_safe_child);
    assert_eq!(state.sync_state.safe_head(), expected_safe_child);

    let storage = client.storage();
    let requests = storage.read().await;
    assert!(
        requests
            .fork_choice_updated_v2_requests
            .iter()
            .chain(requests.fork_choice_updated_v3_requests.iter())
            .all(|(_, has_attrs)| !has_attrs),
        "span-ending safe reconciliation must not start a stale FCU-with-attributes build"
    );
}

#[tokio::test]
async fn consolidate_rejects_attribute_transaction_with_trailing_bytes() {
    let safe_head = crate::test_utils::test_block_info(0);
    let tx = l1_info_deposit_tx();
    let mut attr_tx = Vec::new();
    tx.encode_2718(&mut attr_tx);
    attr_tx.extend_from_slice(b"trailing bytes");

    let attributes = TestAttributesBuilder::new()
        .with_parent(safe_head)
        .with_transactions(vec![attr_tx.into()])
        .build();
    let block_number = attributes.block_number();

    let mut unsafe_block = RpcBlock::<BaseTransaction>::default();
    unsafe_block.header.inner.number = block_number;
    unsafe_block.header.inner.parent_hash = safe_head.block_info.hash;
    unsafe_block.header.inner.timestamp = attributes.attributes().payload_attributes.timestamp;
    unsafe_block.header.inner.mix_hash = attributes.attributes().payload_attributes.prev_randao;
    unsafe_block.header.inner.gas_limit = attributes.attributes().gas_limit.unwrap_or_default();
    unsafe_block.header.inner.parent_beacon_block_root =
        attributes.attributes().payload_attributes.parent_beacon_block_root;
    unsafe_block.transactions = BlockTransactions::Full(vec![rpc_transaction(tx, block_number)]);

    let cfg = RollupConfig::default();
    assert_eq!(
        AttributesMatch::check(&cfg, &attributes, &unsafe_block),
        AttributesMismatch::MalformedAttributesTransaction.into()
    );

    let mut state = TestEngineStateBuilder::new()
        .with_safe_head(safe_head)
        .with_unsafe_head(crate::test_utils::test_block_info(block_number))
        .build();
    let original_safe_head = state.sync_state.safe_head();
    let original_local_safe_head = state.sync_state.local_safe_head();
    let client = Arc::new(
        test_engine_client_builder()
            .with_l2_block_by_label(BlockNumberOrTag::Number(block_number), unsafe_block)
            .build(),
    );
    let task = ConsolidateTask::new(client, Arc::new(cfg), ConsolidateInput::from(attributes));

    let result = task.execute(&mut state).await;

    assert!(result.is_err());
    assert_eq!(state.sync_state.safe_head(), original_safe_head);
    assert_eq!(state.sync_state.local_safe_head(), original_local_safe_head);
}
