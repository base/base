use std::{
    num::NonZeroU64,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use alloy_primitives::B256;
use alloy_rpc_types_engine::ExecutionPayloadV1;
use alloy_transport::TransportErrorKind;
use base_common_genesis::{ChainGenesis, RollupConfig};
use base_common_rpc_types_engine::{
    BaseExecutionPayload, BaseExecutionPayloadEnvelope, BasePayloadAttributes,
};
use base_consensus_derive::{BuilderError, PipelineErrorKind, test_utils::TestAttributesBuilder};
use base_consensus_engine::SealTaskError;
use base_protocol::{AttributesWithParent, BlockInfo, L2BlockInfo};
use jsonrpsee::core::ClientError;
use rstest::rstest;
use tokio::sync::{mpsc, oneshot};

use crate::{
    ConductorError, L1OriginSelectorError, NodeActor, ResetReason, ScheduledTicker, SealState,
    SealStepError, SealStepOutcome, SequencerActorError, SequencerAdminQuery,
    UnsafePayloadGossipClientError, UnsealedPayloadHandle,
    actors::{
        MockConductor, MockOriginSelector, MockSequencerEngineClient,
        MockUnsafePayloadGossipClient,
        engine::EngineClientError,
        sequencer::{PayloadSealer, tests::test_util::test_actor},
    },
};

fn dummy_envelope() -> BaseExecutionPayloadEnvelope {
    BaseExecutionPayloadEnvelope {
        parent_beacon_block_root: None,
        execution_payload: BaseExecutionPayload::V1(ExecutionPayloadV1 {
            parent_hash: B256::ZERO,
            fee_recipient: alloy_primitives::Address::ZERO,
            state_root: B256::ZERO,
            receipts_root: B256::ZERO,
            logs_bloom: alloy_primitives::Bloom::ZERO,
            prev_randao: B256::ZERO,
            block_number: 1,
            gas_limit: 0,
            gas_used: 0,
            timestamp: 0,
            extra_data: alloy_primitives::Bytes::new(),
            base_fee_per_gas: alloy_primitives::U256::ZERO,
            block_hash: B256::ZERO,
            transactions: vec![],
        }),
    }
}

fn conductor_rpc_error() -> ConductorError {
    ConductorError::Rpc(ClientError::Custom("test conductor error".to_string()))
}

fn dummy_attributes_with_parent() -> AttributesWithParent {
    AttributesWithParent::new(BasePayloadAttributes::default(), L2BlockInfo::default(), None, false)
}

fn handle_with_parent_number(number: u64) -> UnsealedPayloadHandle {
    handle_with_parent(number, B256::ZERO)
}

fn handle_with_parent(number: u64, hash: B256) -> UnsealedPayloadHandle {
    let parent = L2BlockInfo {
        block_info: BlockInfo { number, hash, ..Default::default() },
        ..Default::default()
    };
    UnsealedPayloadHandle {
        payload_id: Default::default(),
        attributes_with_parent: AttributesWithParent::new(
            BasePayloadAttributes::default(),
            parent,
            None,
            false,
        ),
    }
}

fn head_at(number: u64) -> L2BlockInfo {
    head_at_with_hash(number, B256::ZERO)
}

fn head_at_with_hash(number: u64, hash: B256) -> L2BlockInfo {
    L2BlockInfo {
        block_info: BlockInfo { number, hash, ..Default::default() },
        ..Default::default()
    }
}

fn head_at_timestamp(number: u64, hash: B256, timestamp: u64) -> L2BlockInfo {
    L2BlockInfo {
        block_info: BlockInfo { number, hash, timestamp, ..Default::default() },
        ..Default::default()
    }
}

fn attributes_at(timestamp: u64) -> BasePayloadAttributes {
    let mut attributes = BasePayloadAttributes::default();
    attributes.payload_attributes.timestamp = timestamp;
    attributes
}

#[rstest]
#[case::no_previous_seal(Duration::ZERO)]
#[case::short_previous_seal(Duration::from_millis(5))]
#[case::long_previous_seal(Duration::from_millis(500))]
#[tokio::test]
async fn test_parent_build_target_overrides_variable_seal_schedule(
    #[case] previous_seal_duration: Duration,
) {
    let parent_timestamp = 2_000_000_000;
    let mut ticker = ScheduledTicker::new(Duration::from_secs(2));

    ticker.reset_before_unix_timestamp(parent_timestamp + 2, previous_seal_duration);
    ticker.reset_at_unix_timestamp(parent_timestamp);

    assert_eq!(ticker.target(), Some(UNIX_EPOCH + Duration::from_secs(parent_timestamp)));
}

#[rstest]
#[case::on_time(0)]
#[case::late(1)]
#[tokio::test(start_paused = true)]
async fn test_on_time_or_late_parent_build_target_is_immediately_runnable(
    #[case] seconds_ago: u64,
) {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    let mut ticker = ScheduledTicker::new(Duration::from_secs(2));

    ticker.reset_at_unix_timestamp(now.saturating_sub(seconds_ago));

    tokio::time::timeout(Duration::from_millis(1), ticker.tick()).await.unwrap();
}

#[rstest]
#[case::on_time(0)]
#[case::late(1)]
#[tokio::test(start_paused = true)]
async fn test_on_time_or_late_insert_starts_child_build_immediately(#[case] seconds_ago: u64) {
    let block_time = 2;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    let inserted_timestamp = now.saturating_sub(seconds_ago);
    let initial_timestamp = inserted_timestamp.saturating_sub(block_time);
    let initial_head = head_at_timestamp(10, B256::with_last_byte(10), initial_timestamp);
    let inserted_head = head_at_timestamp(11, B256::with_last_byte(11), inserted_timestamp);

    let (build_tx, mut build_rx) = mpsc::unbounded_channel();

    let mut client = MockSequencerEngineClient::new();
    client.expect_reset_engine_forkchoice().times(1).return_once(|_| Ok(()));
    client.expect_get_unsafe_head().times(2).returning(move || Ok(initial_head));
    client.expect_start_build_block().times(2).returning(move |attributes| {
        build_tx.send(attributes.parent().block_info.number).unwrap();
        Ok(Default::default())
    });
    client.expect_get_sealed_payload().times(1).return_once(|_, _| Ok(dummy_envelope()));
    client.expect_insert_unsafe_payload().times(1).return_once(move |_| Ok(inserted_head));

    let mut origin_selector = MockOriginSelector::new();
    origin_selector.expect_next_l1_origin().times(2).returning(|_| Ok(BlockInfo::default()));

    let mut gossip = MockUnsafePayloadGossipClient::new();
    gossip.expect_schedule_execution_payload_gossip().times(1).return_once(|_| Ok(()));

    let rollup_config = Arc::new(base_common_genesis::RollupConfig {
        block_time,
        genesis: ChainGenesis {
            l2_time: initial_head
                .block_info
                .timestamp
                .saturating_sub(initial_head.block_info.number.saturating_mul(block_time)),
            ..Default::default()
        },
        ..Default::default()
    });
    let engine_client = Arc::new(client);

    let mut actor = test_actor();
    actor.builder.attributes_builder = TestAttributesBuilder {
        attributes: vec![
            Ok(attributes_at(inserted_timestamp + block_time)),
            Ok(attributes_at(inserted_timestamp)),
        ],
    };
    actor.builder.engine_client = Arc::clone(&engine_client);
    actor.builder.origin_selector = origin_selector;
    actor.builder.rollup_config = Arc::clone(&rollup_config);
    actor.engine_client = engine_client;
    actor.rollup_config = rollup_config;
    actor.unsafe_payload_gossip_client = gossip;

    let cancellation_token = actor.cancellation_token.clone();
    let actor_task = tokio::spawn(actor.start(()));

    assert_eq!(build_rx.recv().await.unwrap(), initial_head.block_info.number);
    assert_eq!(
        tokio::time::timeout(Duration::from_millis(1), build_rx.recv()).await.unwrap().unwrap(),
        inserted_head.block_info.number
    );

    cancellation_token.cancel();
    actor_task.await.unwrap().unwrap();
}

#[tokio::test(start_paused = true)]
async fn test_early_insert_defers_child_build_until_parent_timestamp() {
    let block_time = 2;
    let initial_timestamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    let initial_head = head_at_timestamp(10, B256::with_last_byte(10), initial_timestamp);
    let inserted_head =
        head_at_timestamp(11, B256::with_last_byte(11), initial_timestamp + block_time);

    let (build_tx, mut build_rx) = mpsc::unbounded_channel();
    let (insert_tx, mut insert_rx) = mpsc::unbounded_channel();

    let mut client = MockSequencerEngineClient::new();
    client.expect_reset_engine_forkchoice().times(1).return_once(|_| Ok(()));
    client.expect_get_unsafe_head().times(2).returning(move || Ok(initial_head));
    client.expect_start_build_block().times(2).returning(move |attributes| {
        build_tx.send(attributes.parent().block_info.number).unwrap();
        Ok(Default::default())
    });
    client.expect_get_sealed_payload().times(1).return_once(|_, _| Ok(dummy_envelope()));
    client.expect_insert_unsafe_payload().times(1).return_once(move |_| {
        insert_tx.send(()).unwrap();
        Ok(inserted_head)
    });

    let mut origin_selector = MockOriginSelector::new();
    origin_selector.expect_next_l1_origin().times(2).returning(|_| Ok(BlockInfo::default()));

    let mut gossip = MockUnsafePayloadGossipClient::new();
    gossip.expect_schedule_execution_payload_gossip().times(1).return_once(|_| Ok(()));

    let rollup_config = Arc::new(base_common_genesis::RollupConfig {
        block_time,
        genesis: ChainGenesis {
            l2_time: initial_head
                .block_info
                .timestamp
                .saturating_sub(initial_head.block_info.number.saturating_mul(block_time)),
            ..Default::default()
        },
        ..Default::default()
    });
    let engine_client = Arc::new(client);

    let mut actor = test_actor();
    actor.builder.attributes_builder = TestAttributesBuilder {
        attributes: vec![
            Ok(attributes_at(initial_timestamp + 2 * block_time)),
            Ok(attributes_at(initial_timestamp + block_time)),
        ],
    };
    actor.builder.engine_client = Arc::clone(&engine_client);
    actor.builder.origin_selector = origin_selector;
    actor.builder.rollup_config = Arc::clone(&rollup_config);
    actor.engine_client = engine_client;
    actor.rollup_config = rollup_config;
    actor.unsafe_payload_gossip_client = gossip;

    let cancellation_token = actor.cancellation_token.clone();
    let actor_task = tokio::spawn(actor.start(()));

    assert_eq!(build_rx.recv().await.unwrap(), initial_head.block_info.number);

    tokio::time::advance(Duration::from_secs(block_time)).await;
    insert_rx.recv().await.unwrap();
    tokio::task::yield_now().await;

    assert!(build_rx.try_recv().is_err());

    tokio::time::advance(Duration::from_millis(500)).await;
    tokio::task::yield_now().await;
    assert!(build_rx.try_recv().is_err());

    tokio::time::advance(Duration::from_secs(block_time)).await;
    assert_eq!(build_rx.recv().await.unwrap(), inserted_head.block_info.number);

    cancellation_token.cancel();
    actor_task.await.unwrap().unwrap();
}

#[tokio::test(start_paused = true)]
async fn test_stop_discards_queued_parent_and_restart_builds_immediately_on_fresh_head() {
    let block_time = 2;
    let initial_timestamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    let initial_head = head_at_timestamp(20, B256::with_last_byte(20), initial_timestamp);
    let inserted_head =
        head_at_timestamp(21, B256::with_last_byte(21), initial_timestamp + block_time);
    let restart_head =
        head_at_timestamp(22, B256::with_last_byte(22), initial_timestamp + 2 * block_time);

    let (build_tx, mut build_rx) = mpsc::unbounded_channel();
    let (insert_tx, mut insert_rx) = mpsc::unbounded_channel();
    let get_head_calls = Arc::new(AtomicUsize::new(0));

    let mut client = MockSequencerEngineClient::new();
    client.expect_reset_engine_forkchoice().times(1).return_once(|_| Ok(()));
    client.expect_get_unsafe_head().times(5).returning({
        let get_head_calls = Arc::clone(&get_head_calls);
        move || {
            let call = get_head_calls.fetch_add(1, Ordering::Relaxed);
            Ok(match call {
                0 | 1 => initial_head,
                2 => inserted_head,
                _ => restart_head,
            })
        }
    });
    client.expect_start_build_block().times(2).returning(move |attributes| {
        build_tx.send(attributes.parent().block_info.number).unwrap();
        Ok(Default::default())
    });
    client.expect_get_sealed_payload().times(1).return_once(|_, _| Ok(dummy_envelope()));
    client.expect_insert_unsafe_payload().times(1).return_once(move |_| {
        insert_tx.send(()).unwrap();
        Ok(inserted_head)
    });

    let mut origin_selector = MockOriginSelector::new();
    origin_selector.expect_next_l1_origin().times(2).returning(|_| Ok(BlockInfo::default()));

    let mut gossip = MockUnsafePayloadGossipClient::new();
    gossip.expect_schedule_execution_payload_gossip().times(1).return_once(|_| Ok(()));

    let rollup_config = Arc::new(base_common_genesis::RollupConfig {
        block_time,
        genesis: ChainGenesis {
            l2_time: initial_head
                .block_info
                .timestamp
                .saturating_sub(initial_head.block_info.number.saturating_mul(block_time)),
            ..Default::default()
        },
        ..Default::default()
    });
    let engine_client = Arc::new(client);

    let (admin_api_tx, admin_api_rx) = mpsc::channel(4);
    let mut actor = test_actor();
    actor.admin_api_rx = admin_api_rx;
    actor.builder.attributes_builder = TestAttributesBuilder {
        attributes: vec![
            Ok(attributes_at(restart_head.block_info.timestamp + block_time)),
            Ok(attributes_at(inserted_head.block_info.timestamp)),
        ],
    };
    actor.builder.engine_client = Arc::clone(&engine_client);
    actor.builder.origin_selector = origin_selector;
    actor.builder.rollup_config = Arc::clone(&rollup_config);
    actor.engine_client = engine_client;
    actor.rollup_config = rollup_config;
    actor.unsafe_payload_gossip_client = gossip;

    let cancellation_token = actor.cancellation_token.clone();
    let actor_task = tokio::spawn(actor.start(()));

    assert_eq!(build_rx.recv().await.unwrap(), initial_head.block_info.number);
    tokio::time::advance(Duration::from_secs(block_time)).await;
    insert_rx.recv().await.unwrap();
    tokio::task::yield_now().await;

    let (stop_tx, stop_rx) = oneshot::channel();
    admin_api_tx.send(SequencerAdminQuery::StopSequencer(stop_tx)).await.unwrap();
    assert_eq!(stop_rx.await.unwrap().unwrap(), inserted_head.block_info.hash);

    tokio::time::advance(Duration::from_secs(block_time + 1)).await;
    tokio::task::yield_now().await;
    assert!(build_rx.try_recv().is_err());

    let (start_tx, start_rx) = oneshot::channel();
    admin_api_tx
        .send(SequencerAdminQuery::StartSequencer(restart_head.block_info.hash, start_tx))
        .await
        .unwrap();
    start_rx.await.unwrap().unwrap();

    assert_eq!(
        tokio::time::timeout(Duration::from_millis(1), build_rx.recv()).await.unwrap().unwrap(),
        restart_head.block_info.number
    );

    cancellation_token.cancel();
    actor_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn shadow_cycle_reconciles_after_configured_private_block_count() {
    let cycle_start = head_at(0);
    let private_head = head_at(1);
    let cancellation_token = tokio_util::sync::CancellationToken::new();
    let cancel_after_reconciliation = cancellation_token.clone();
    let mut client = MockSequencerEngineClient::new();
    client.expect_reset_engine_forkchoice_coordinated().times(1).return_once(|_| Ok(()));
    client.expect_get_unsafe_head().times(1).return_once(move || Ok(cycle_start));
    client.expect_insert_unsafe_payload().times(1).return_once(move |_| Ok(private_head));
    client
        .expect_reconcile_shadow()
        .withf(move |target| *target == private_head)
        .times(1)
        .return_once(move |_| {
            cancel_after_reconciliation.cancel();
            Ok(None)
        });

    let mut actor = test_actor();
    let rollup_config = Arc::new(RollupConfig { block_time: 2, ..Default::default() });
    actor.cancellation_token = cancellation_token;
    actor.engine_client = Arc::new(client);
    actor.builder.rollup_config = Arc::clone(&rollup_config);
    actor.rollup_config = rollup_config;
    actor.shadow_blocks_per_cycle = NonZeroU64::new(1);
    actor.sealer = Some(PayloadSealer::new_private(dummy_envelope()));

    actor.start(()).await.unwrap();
}

// --- try_seal_handle tests ---

#[tokio::test]
async fn test_try_seal_handle_current_head_equals_parent_seals() {
    // head.number == parent.number AND head.hash == parent.hash → not stale; seal proceeds.
    // Use a distinct non-zero hash so the hash equality check is actually exercised.
    let hash = B256::from([0xcc; 32]);

    let mut client = MockSequencerEngineClient::new();
    client.expect_get_unsafe_head().times(1).return_once(move || Ok(head_at_with_hash(5, hash)));
    client.expect_get_sealed_payload().times(1).return_once(|_, _| Ok(dummy_envelope()));

    let mut actor = test_actor();
    actor.engine_client = Arc::new(client);

    let (sealer, dur) = actor.try_seal_handle(handle_with_parent(5, hash)).await.unwrap().unwrap();
    assert_eq!(sealer.state, SealState::Sealed);
    assert!(dur < Duration::from_secs(10));
}

#[tokio::test]
async fn test_try_seal_handle_current_head_ahead_of_parent_discards() {
    // head > parent → stale; seal_payload must NOT be called.
    let mut client = MockSequencerEngineClient::new();
    client.expect_get_unsafe_head().times(1).return_once(|| Ok(head_at(6)));
    client.expect_get_sealed_payload().times(0);

    let mut actor = test_actor();
    actor.engine_client = Arc::new(client);

    let result = actor.try_seal_handle(handle_with_parent_number(5)).await;

    assert!(result.unwrap().is_none());
}

#[tokio::test]
async fn test_try_seal_handle_same_height_reorg_discards() {
    // head.number == parent.number but head.hash != parent.hash → same-height reorg; discard.
    let parent_hash = B256::from([0xaa; 32]);
    let reorged_hash = B256::from([0xbb; 32]);

    let mut client = MockSequencerEngineClient::new();
    client
        .expect_get_unsafe_head()
        .times(1)
        .return_once(move || Ok(head_at_with_hash(5, reorged_hash)));
    client.expect_get_sealed_payload().times(0);

    let mut actor = test_actor();
    actor.engine_client = Arc::new(client);

    let result = actor.try_seal_handle(handle_with_parent(5, parent_hash)).await;

    assert!(result.unwrap().is_none());
}

#[tokio::test]
async fn test_try_seal_handle_get_unsafe_head_error_propagates() {
    let mut client = MockSequencerEngineClient::new();
    client
        .expect_get_unsafe_head()
        .times(1)
        .return_once(|| Err(EngineClientError::RequestError("channel closed".to_string())));
    client.expect_get_sealed_payload().times(0);

    let mut actor = test_actor();
    actor.engine_client = Arc::new(client);

    let result = actor.try_seal_handle(handle_with_parent_number(5)).await;

    assert!(result.is_err());
}

#[tokio::test]
async fn test_try_seal_handle_fatal_seal_error_cancels_and_propagates() {
    // A fatal seal error must cancel the token and return Err.
    let mut client = MockSequencerEngineClient::new();
    client.expect_get_unsafe_head().times(1).return_once(|| Ok(head_at(5)));
    client.expect_get_sealed_payload().times(1).return_once(|_, _| {
        Err(EngineClientError::SealError(SealTaskError::DepositOnlyPayloadFailed))
    });

    let mut actor = test_actor();
    actor.engine_client = Arc::new(client);

    let result = actor.try_seal_handle(handle_with_parent_number(5)).await;

    assert!(result.is_err());
    assert!(actor.cancellation_token.is_cancelled());
}

#[tokio::test]
async fn test_try_seal_handle_non_fatal_seal_error_returns_none() {
    // A non-fatal seal error must return Ok(None) and leave the token uncancelled.
    let mut client = MockSequencerEngineClient::new();
    client.expect_get_unsafe_head().times(1).return_once(|| Ok(head_at(5)));
    client
        .expect_get_sealed_payload()
        .times(1)
        .return_once(|_, _| Err(EngineClientError::SealError(SealTaskError::HoloceneInvalidFlush)));

    let mut actor = test_actor();
    actor.engine_client = Arc::new(client);

    let result = actor.try_seal_handle(handle_with_parent_number(5)).await;

    assert!(result.unwrap().is_none());
    assert!(!actor.cancellation_token.is_cancelled());
}

#[rstest]
#[case::awaiting_l1_origin(false, false)]
#[case::provider_error(true, false)]
#[case::repeated_orphan_resets(false, true)]
#[tokio::test(start_paused = true)]
async fn test_build_retries_are_paced_after_immediate_budget(
    #[case] provider_error: bool,
    #[case] orphaned: bool,
) {
    let attempts_before_delay = usize::from(ScheduledTicker::MAX_IMMEDIATE_L1_ORIGIN_RETRIES) + 1;
    let expected_attempts = attempts_before_delay + 1;
    let (attempt_tx, mut attempt_rx) = mpsc::unbounded_channel();

    let mut client = MockSequencerEngineClient::new();
    let expected_resets = if orphaned { expected_attempts + 1 } else { 1 };
    client.expect_reset_engine_forkchoice().times(expected_resets).returning(|_| Ok(()));
    client
        .expect_get_unsafe_head()
        .times(expected_attempts)
        .returning(|| Ok(L2BlockInfo::default()));
    client.expect_start_build_block().times(0);

    let mut origin_selector = MockOriginSelector::new();
    origin_selector.expect_next_l1_origin().times(expected_attempts).returning(move |_| {
        attempt_tx.send(()).unwrap();
        if orphaned {
            Err(L1OriginSelectorError::NextL1OriginOrphaned {
                current: B256::with_last_byte(1),
                next: B256::with_last_byte(2),
            })
        } else if provider_error {
            Err(L1OriginSelectorError::Provider(TransportErrorKind::custom_str(
                "mock L1 provider failure",
            )))
        } else {
            Err(L1OriginSelectorError::NotEnoughData(BlockInfo::default()))
        }
    });

    let engine_client = Arc::new(client);
    let rollup_config = Arc::new(RollupConfig { block_time: 2, ..Default::default() });
    let mut actor = test_actor();
    actor.builder.engine_client = Arc::clone(&engine_client);
    actor.builder.origin_selector = origin_selector;
    actor.builder.rollup_config = Arc::clone(&rollup_config);
    actor.engine_client = engine_client;
    actor.rollup_config = rollup_config;

    let cancellation_token = actor.cancellation_token.clone();
    let actor_task = tokio::spawn(actor.start(()));

    // The initial attempt and five retries run immediately to absorb a near-complete fetch.
    for _ in 0..attempts_before_delay {
        attempt_rx.recv().await.unwrap();
    }
    tokio::task::yield_now().await;
    assert!(attempt_rx.try_recv().is_err());

    tokio::time::advance(ScheduledTicker::L1_ORIGIN_RETRY_DELAY - Duration::from_millis(50)).await;
    tokio::task::yield_now().await;
    assert!(attempt_rx.try_recv().is_err());

    tokio::time::advance(Duration::from_millis(50)).await;
    attempt_rx.recv().await.unwrap();

    cancellation_token.cancel();
    actor_task.await.unwrap().unwrap();
}

// --- build tests ---

#[tokio::test]
async fn test_orphaned_l1_origin_resets_once_without_starting_block_build() {
    let unsafe_head = L2BlockInfo::default();
    let mut client = MockSequencerEngineClient::new();
    client.expect_get_unsafe_head().times(1).return_once(move || Ok(unsafe_head));
    client
        .expect_reset_engine_forkchoice()
        .with(mockall::predicate::eq(ResetReason::L1OriginOrphaned))
        .times(1)
        .return_once(|_| Ok(()));
    client.expect_start_build_block().times(0);

    let mut origin_selector = MockOriginSelector::new();
    origin_selector.expect_next_l1_origin().times(1).return_once(|_| {
        Err(L1OriginSelectorError::NextL1OriginOrphaned {
            current: B256::with_last_byte(1),
            next: B256::with_last_byte(2),
        })
    });

    let mut actor = test_actor();
    actor.builder.origin_selector = origin_selector;
    actor.builder.engine_client = Arc::new(client);

    assert!(matches!(actor.builder.build().await.unwrap(), crate::BuildOutcome::Deferred));
}

#[tokio::test]
async fn test_orphaned_l1_origin_propagates_engine_reset_failure() {
    let unsafe_head = L2BlockInfo::default();
    let mut client = MockSequencerEngineClient::new();
    client.expect_get_unsafe_head().times(1).return_once(move || Ok(unsafe_head));
    client
        .expect_reset_engine_forkchoice()
        .with(mockall::predicate::eq(ResetReason::L1OriginOrphaned))
        .times(1)
        .return_once(|_| Err(EngineClientError::ResetForkchoiceError("mock reset failure".into())));
    client.expect_start_build_block().times(0);

    let mut origin_selector = MockOriginSelector::new();
    origin_selector.expect_next_l1_origin().times(1).return_once(|_| {
        Err(L1OriginSelectorError::NextL1OriginOrphaned {
            current: B256::with_last_byte(1),
            next: B256::with_last_byte(2),
        })
    });

    let mut actor = test_actor();
    actor.builder.origin_selector = origin_selector;
    actor.builder.engine_client = Arc::new(client);

    assert!(matches!(
        actor.builder.build().await,
        Err(SequencerActorError::EngineError(EngineClientError::ResetForkchoiceError(error)))
            if error == "mock reset failure"
    ));
}

#[rstest]
#[case::temp(PipelineErrorKind::Temporary(BuilderError::Custom(String::new()).into()), false)]
#[case::reset(PipelineErrorKind::Reset(BuilderError::Custom(String::new()).into()), false)]
#[case::critical(PipelineErrorKind::Critical(BuilderError::Custom(String::new()).into()), true)]
#[tokio::test]
async fn test_build_unsealed_payload_prepare_payload_attributes_error(
    #[case] forced_error: PipelineErrorKind,
    #[case] expect_err: bool,
) {
    let mut client = MockSequencerEngineClient::new();

    let unsafe_head = L2BlockInfo::default();
    client.expect_get_unsafe_head().times(1).return_once(move || Ok(unsafe_head));
    client.expect_start_build_block().times(0);
    // Reset pipeline errors no longer trigger engine reset — the attributes builder is stateless
    // so resetting the engine would only rewind the unsafe head without aiding recovery.
    client.expect_reset_engine_forkchoice().times(0);

    let l1_origin = BlockInfo::default();
    let mut origin_selector = MockOriginSelector::new();
    origin_selector.expect_next_l1_origin().times(1).return_once(move |_| Ok(l1_origin));

    let attributes_builder = TestAttributesBuilder { attributes: vec![Err(forced_error)] };

    let mut actor = test_actor();
    actor.builder.origin_selector = origin_selector;
    actor.builder.engine_client = Arc::new(client);
    actor.builder.attributes_builder = attributes_builder;

    let result = actor.builder.build().await;
    if expect_err {
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            SequencerActorError::AttributesBuilder(PipelineErrorKind::Critical(_))
        ));
    } else {
        assert!(result.is_ok());
    }
}

// --- seal_payload tests ---

#[tokio::test]
async fn test_seal_payload_success_returns_sealer() {
    let envelope = dummy_envelope();

    let mut client = MockSequencerEngineClient::new();
    client.expect_get_sealed_payload().times(1).return_once(move |_, _| Ok(envelope));

    let mut actor = test_actor();
    actor.engine_client = Arc::new(client);

    let handle = UnsealedPayloadHandle {
        payload_id: Default::default(),
        attributes_with_parent: dummy_attributes_with_parent(),
    };
    let sealer = actor.seal_payload(&handle).await;

    assert!(sealer.is_ok());
    assert_eq!(sealer.unwrap().state, SealState::Sealed);
}

#[tokio::test]
async fn test_shadow_seal_payload_returns_private_sealer() {
    let envelope = dummy_envelope();

    let mut client = MockSequencerEngineClient::new();
    client.expect_get_sealed_payload().times(1).return_once(move |_, _| Ok(envelope));

    let mut actor = test_actor();
    actor.engine_client = Arc::new(client);
    actor.shadow_blocks_per_cycle = NonZeroU64::new(10);

    let handle = UnsealedPayloadHandle {
        payload_id: Default::default(),
        attributes_with_parent: dummy_attributes_with_parent(),
    };
    let sealer = actor.seal_payload(&handle).await.unwrap();

    assert_eq!(sealer.state, SealState::Private);
}

#[tokio::test]
async fn test_seal_payload_failure_propagates() {
    let mut client = MockSequencerEngineClient::new();
    client
        .expect_get_sealed_payload()
        .times(1)
        .return_once(|_, _| Err(EngineClientError::RequestError("engine offline".to_string())));

    let mut actor = test_actor();
    actor.engine_client = Arc::new(client);

    let handle = UnsealedPayloadHandle {
        payload_id: Default::default(),
        attributes_with_parent: dummy_attributes_with_parent(),
    };
    let result = actor.seal_payload(&handle).await;

    assert!(result.is_err());
}

// --- PayloadSealer::step tests ---

#[tokio::test]
async fn test_private_sealer_only_inserts() {
    let envelope = dummy_envelope();

    let mut conductor = MockConductor::new();
    conductor.expect_commit_unsafe_payload().times(0);

    let mut gossip = MockUnsafePayloadGossipClient::new();
    gossip.expect_schedule_execution_payload_gossip().times(0);

    let mut engine = MockSequencerEngineClient::new();
    engine.expect_insert_unsafe_payload().times(1).return_once(|_| Ok(L2BlockInfo::default()));

    let mut sealer = PayloadSealer::new_private(envelope);
    let result = sealer.step(&Some(conductor), &gossip, &engine).await;

    assert_eq!(result.unwrap(), SealStepOutcome::Inserted(L2BlockInfo::default()));
    assert_eq!(sealer.state, SealState::Private);
}

#[tokio::test]
async fn test_private_sealer_insert_failure_stays_private() {
    let envelope = dummy_envelope();

    let mut conductor = MockConductor::new();
    conductor.expect_commit_unsafe_payload().times(0);

    let mut gossip = MockUnsafePayloadGossipClient::new();
    gossip.expect_schedule_execution_payload_gossip().times(0);

    let mut engine = MockSequencerEngineClient::new();
    engine
        .expect_insert_unsafe_payload()
        .times(1)
        .return_once(|_| Err(EngineClientError::RequestError("channel closed".to_string())));

    let mut sealer = PayloadSealer::new_private(envelope);
    let result = sealer.step(&Some(conductor), &gossip, &engine).await;

    assert!(matches!(result.unwrap_err(), SealStepError::Insert(_)));
    assert_eq!(sealer.state, SealState::Private);
}

#[tokio::test]
async fn test_sealer_full_pipeline_no_conductor() {
    let envelope = dummy_envelope();

    let mut gossip = MockUnsafePayloadGossipClient::new();
    gossip.expect_schedule_execution_payload_gossip().times(1).return_once(|_| Ok(()));

    let mut engine = MockSequencerEngineClient::new();
    engine.expect_insert_unsafe_payload().times(1).return_once(|_| Ok(L2BlockInfo::default()));

    let conductor: Option<MockConductor> = None;
    let mut sealer = PayloadSealer::new(envelope);

    assert_eq!(sealer.state, SealState::Sealed);

    let result = sealer.step(&conductor, &gossip, &engine).await;
    assert_eq!(result.unwrap(), SealStepOutcome::Pending);
    assert_eq!(sealer.state, SealState::Committed);

    let result = sealer.step(&conductor, &gossip, &engine).await;
    assert_eq!(result.unwrap(), SealStepOutcome::Pending);
    assert_eq!(sealer.state, SealState::Gossiped);

    let result = sealer.step(&conductor, &gossip, &engine).await;
    assert_eq!(result.unwrap(), SealStepOutcome::Inserted(L2BlockInfo::default()));
}

#[tokio::test]
async fn test_sealer_full_pipeline_with_conductor() {
    let envelope = dummy_envelope();

    let mut conductor = MockConductor::new();
    conductor.expect_commit_unsafe_payload().times(1).return_once(|_| Ok(()));

    let mut gossip = MockUnsafePayloadGossipClient::new();
    gossip.expect_schedule_execution_payload_gossip().times(1).return_once(|_| Ok(()));

    let mut engine = MockSequencerEngineClient::new();
    engine.expect_insert_unsafe_payload().times(1).return_once(|_| Ok(L2BlockInfo::default()));

    let conductor = Some(conductor);
    let mut sealer = PayloadSealer::new(envelope);

    let result = sealer.step(&conductor, &gossip, &engine).await;
    assert_eq!(result.unwrap(), SealStepOutcome::Pending);
    assert_eq!(sealer.state, SealState::Committed);

    let result = sealer.step(&conductor, &gossip, &engine).await;
    assert_eq!(result.unwrap(), SealStepOutcome::Pending);
    assert_eq!(sealer.state, SealState::Gossiped);

    let result = sealer.step(&conductor, &gossip, &engine).await;
    assert_eq!(result.unwrap(), SealStepOutcome::Inserted(L2BlockInfo::default()));
}

#[tokio::test]
async fn test_sealer_conductor_failure_stays_sealed() {
    let envelope = dummy_envelope();

    let mut conductor = MockConductor::new();
    conductor.expect_commit_unsafe_payload().times(1).return_once(|_| Err(conductor_rpc_error()));

    let gossip = MockUnsafePayloadGossipClient::new();
    let engine = MockSequencerEngineClient::new();

    let conductor = Some(conductor);
    let mut sealer = PayloadSealer::new(envelope);

    let result = sealer.step(&conductor, &gossip, &engine).await;
    assert!(result.is_err());
    assert!(matches!(result.unwrap_err(), SealStepError::Conductor(_)));
    assert_eq!(sealer.state, SealState::Sealed);
}

#[tokio::test]
async fn test_sealer_gossip_failure_stays_committed() {
    let envelope = dummy_envelope();

    let mut gossip = MockUnsafePayloadGossipClient::new();
    gossip.expect_schedule_execution_payload_gossip().times(1).return_once(|_| {
        Err(UnsafePayloadGossipClientError::RequestError("channel closed".to_string()))
    });

    let engine = MockSequencerEngineClient::new();
    let conductor: Option<MockConductor> = None;
    let mut sealer = PayloadSealer::new(envelope);

    let _ = sealer.step(&conductor, &gossip, &engine).await.unwrap();
    assert_eq!(sealer.state, SealState::Committed);

    let result = sealer.step(&conductor, &gossip, &engine).await;
    assert!(result.is_err());
    assert!(matches!(result.unwrap_err(), SealStepError::Gossip(_)));
    assert_eq!(sealer.state, SealState::Committed);
}

#[tokio::test]
async fn test_sealer_insert_failure_stays_gossiped() {
    let envelope = dummy_envelope();

    let mut gossip = MockUnsafePayloadGossipClient::new();
    gossip.expect_schedule_execution_payload_gossip().times(1).return_once(|_| Ok(()));

    let mut engine = MockSequencerEngineClient::new();
    engine
        .expect_insert_unsafe_payload()
        .times(1)
        .return_once(|_| Err(EngineClientError::RequestError("channel closed".to_string())));

    let conductor: Option<MockConductor> = None;
    let mut sealer = PayloadSealer::new(envelope);

    let _ = sealer.step(&conductor, &gossip, &engine).await.unwrap();
    let _ = sealer.step(&conductor, &gossip, &engine).await.unwrap();
    assert_eq!(sealer.state, SealState::Gossiped);

    let result = sealer.step(&conductor, &gossip, &engine).await;
    assert!(result.is_err());
    assert!(matches!(result.unwrap_err(), SealStepError::Insert(_)));
    assert_eq!(sealer.state, SealState::Gossiped);
}
