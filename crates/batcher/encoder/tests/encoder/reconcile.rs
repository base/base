//! Reconciliation of [`BatchEncoder`] with derivation progress: pruning below the safe L2
//! head, a safe head off the buffered chain, and a fully confirmed channel derivation skipped.

use alloy_primitives::B256;
use base_batcher_encoder::{BatchPipeline, DerivationReconciliation, EncoderConfig, StepResult};
use base_protocol::BlockInfo;

use crate::common::{
    BlockFixture, CHANNEL_TIMEOUT, EncoderFixture, MULTI_FRAME_PAYLOAD, SharedBlob,
    SubmissionFixture,
};

/// Blocks at or below the safe head are pruned whether or not they were encoded yet, and a
/// channel left without blocks goes with them.
#[test]
fn reconcile_prunes_blocks_up_to_the_safe_head() {
    let fixture = EncoderFixture::new(EncoderConfig::default());
    let mut encoder = fixture.encoder();
    let blocks = BlockFixture::chain(2, 0);
    for block in &blocks {
        encoder.add_block(block.clone()).unwrap();
    }
    assert_eq!(encoder.step().unwrap(), StepResult::BlockEncoded);

    assert_eq!(
        encoder.reconcile_derivation(BlockInfo::from(&blocks[1]), None),
        DerivationReconciliation::Consistent
    );

    assert_eq!(encoder.step().unwrap(), StepResult::Idle, "the second block is gone");
    encoder.flush().unwrap();
    assert!(encoder.next_submission().is_none(), "the first block's channel is gone");
    assert_eq!(encoder.da_backlog_bytes(), 0);
}

/// A replay re-encodes only the blocks still above the safe head: the pruned start of the
/// channel is never sent again.
#[test]
fn pruned_blocks_are_not_replayed() {
    let fixture = EncoderFixture::small_calldata_frames();
    let mut encoder = fixture.encoder();
    let blocks = BlockFixture::chain(2, MULTI_FRAME_PAYLOAD);
    for block in &blocks {
        encoder.add_block(block.clone()).unwrap();
    }
    let submissions = encoder.encode_and_drain().unwrap();
    assert!(submissions.len() > 1, "{} submissions", submissions.len());

    encoder.confirm(submissions[0].id, 1);
    encoder.advance_l1_head(1);
    assert_eq!(
        encoder.reconcile_derivation(BlockInfo::from(&blocks[0]), None),
        DerivationReconciliation::Consistent
    );
    encoder.confirm(submissions[1].id, 4);
    encoder.advance_l1_head(4);

    assert_eq!(encoder.step().unwrap(), StepResult::BlockEncoded);
    assert_eq!(encoder.step().unwrap(), StepResult::Idle);
    let replay = encoder.encode_and_drain().unwrap();
    assert_eq!(fixture.derive(&replay).concat(), BlockFixture::batches(&blocks[1..]));
}

/// Pruning a whole channel does not shift the next one: its replay re-encodes its own
/// blocks, not the blocks at its former positions.
#[test]
fn pruned_channel_does_not_shift_the_replay_of_the_next_one() {
    let fixture = EncoderFixture::small_calldata_frames();
    let mut encoder = fixture.encoder();
    let blocks = BlockFixture::chain(2, MULTI_FRAME_PAYLOAD);
    encoder.add_block(blocks[0].clone()).unwrap();
    for submission in encoder.encode_and_drain().unwrap() {
        encoder.confirm(submission.id, 1);
    }
    encoder.add_block(blocks[1].clone()).unwrap();
    let second = encoder.encode_and_drain().unwrap();
    assert!(second.len() > 1, "{} submissions", second.len());
    encoder.confirm(second[0].id, 1);

    assert_eq!(
        encoder.reconcile_derivation(BlockInfo::from(&blocks[0]), None),
        DerivationReconciliation::Consistent
    );
    encoder.advance_l1_head(1 + CHANNEL_TIMEOUT + 1);

    let replay = encoder.encode_and_drain().unwrap();
    assert_eq!(fixture.derive(&replay).concat(), BlockFixture::batches(&blocks[1..]));
}

/// After a prune inside a channel, the channel still maps to the blocks it has left: a stall
/// on its last block is reported, and a safe head covering it is consistent.
#[test]
fn reconcile_follows_a_channel_after_a_prune_inside_it() {
    let fixture = EncoderFixture::new(EncoderConfig::default());
    let mut encoder = fixture.encoder();
    let blocks = BlockFixture::chain(2, 0);
    for block in &blocks {
        encoder.add_block(block.clone()).unwrap();
        assert_eq!(encoder.step().unwrap(), StepResult::BlockEncoded);
    }
    assert_eq!(
        encoder.reconcile_derivation(BlockInfo::from(&blocks[0]), None),
        DerivationReconciliation::Consistent
    );
    for submission in encoder.encode_and_drain().unwrap() {
        encoder.confirm(submission.id, 10);
    }

    assert_eq!(
        encoder.reconcile_derivation(BlockInfo::from(&blocks[0]), Some(11)),
        DerivationReconciliation::StalledChannel
    );
    assert_eq!(
        encoder.reconcile_derivation(BlockInfo::from(&blocks[1]), Some(11)),
        DerivationReconciliation::Consistent
    );
    assert_eq!(encoder.da_backlog_bytes(), 0);
}

/// With nothing buffered, any safe head anchors the chain: the next block must build on it.
#[test]
fn reconcile_with_nothing_buffered_anchors_the_chain() {
    let fixture = EncoderFixture::new(EncoderConfig::default());
    let mut encoder = fixture.encoder();
    let anchor = BlockInfo { hash: B256::repeat_byte(7), number: 2, ..Default::default() };

    assert_eq!(encoder.reconcile_derivation(anchor, None), DerivationReconciliation::Consistent);

    assert!(encoder.add_block(BlockFixture::block(B256::ZERO, 3, 0)).is_err(), "off the anchor");
    encoder.add_block(BlockFixture::block(anchor.hash, 3, 0)).unwrap();
}

/// A safe head that is not on the buffered chain is reported and changes nothing: the parent
/// of the oldest block is the only head below it that fits, an unknown hash or a head above
/// the buffered blocks does not.
#[test]
fn reconcile_reports_a_safe_head_off_the_buffered_chain() {
    let fixture = EncoderFixture::new(EncoderConfig::default());
    let mut encoder = fixture.encoder();
    let anchor = BlockInfo { hash: B256::repeat_byte(7), number: 2, ..Default::default() };
    assert_eq!(encoder.reconcile_derivation(anchor, None), DerivationReconciliation::Consistent);
    let block = BlockFixture::block(anchor.hash, 3, 0);
    encoder.add_block(block.clone()).unwrap();
    assert_eq!(encoder.step().unwrap(), StepResult::BlockEncoded);

    let mismatch = DerivationReconciliation::SafeHeadMismatch;
    let unknown = B256::repeat_byte(1);
    assert_eq!(
        encoder.reconcile_derivation(BlockInfo { hash: unknown, number: 2, ..anchor }, None),
        mismatch
    );
    assert_eq!(
        encoder.reconcile_derivation(BlockInfo { hash: unknown, number: 3, ..anchor }, None),
        mismatch
    );
    assert_eq!(encoder.reconcile_derivation(BlockInfo { number: 1, ..anchor }, None), mismatch);
    assert_eq!(encoder.reconcile_derivation(BlockInfo { number: 4, ..anchor }, None), mismatch);

    let next = BlockFixture::block(block.header.hash_slow(), 4, 0);
    encoder.add_block(next).expect("the buffered chain is untouched");
    assert_eq!(encoder.reconcile_derivation(anchor, None), DerivationReconciliation::Consistent);
}

/// A fully confirmed channel that derivation passed without making its last block safe is
/// reported as stalled, and only then: without a derivation cursor, with the cursor still on
/// the inclusion block, or with a frame still in flight, nothing can be concluded.
#[test]
fn reconcile_reports_a_channel_derivation_passed_without_deriving() {
    let fixture = EncoderFixture::small_calldata_frames();
    let mut encoder = fixture.encoder();
    let inclusion = 1_000;
    let block = BlockFixture::block(B256::ZERO, 101, MULTI_FRAME_PAYLOAD);
    encoder.add_block(block.clone()).unwrap();
    let submissions = encoder.encode_and_drain().unwrap();
    let (last, landed) = submissions.split_last().expect("at least one frame");
    assert!(!landed.is_empty(), "the block must span several frames");
    for submission in landed {
        encoder.confirm(submission.id, inclusion);
    }
    let previous_safe_l2 = BlockInfo {
        hash: block.header.parent_hash,
        number: block.header.number - 1,
        ..Default::default()
    };

    assert_eq!(
        encoder.reconcile_derivation(previous_safe_l2, Some(inclusion + 1)),
        DerivationReconciliation::Consistent,
        "the last frame is still in flight"
    );
    encoder.confirm(last.id, inclusion);
    assert_eq!(
        encoder.reconcile_derivation(previous_safe_l2, None),
        DerivationReconciliation::Consistent,
        "no derivation cursor"
    );
    assert_eq!(
        encoder.reconcile_derivation(previous_safe_l2, Some(inclusion)),
        DerivationReconciliation::Consistent,
        "the inclusion block may still be processing"
    );
    assert_eq!(
        encoder.reconcile_derivation(previous_safe_l2, Some(inclusion + 1)),
        DerivationReconciliation::StalledChannel
    );
    assert_eq!(
        encoder.reconcile_derivation(BlockInfo::from(&block), Some(inclusion + 1)),
        DerivationReconciliation::Consistent,
        "the channel was derived after all"
    );
}

/// A blob shared by a pruned channel and the next one keeps serving the next one: its retry
/// carries the same frames, and derivation reads both channels from what landed.
#[test]
fn shared_blob_keeps_serving_the_remaining_channel_after_a_prune() {
    let config = EncoderConfig { compressed_size_target: Some(1), ..EncoderConfig::default() };
    let fixture = EncoderFixture::new(config);
    let mut encoder = fixture.encoder();
    let shared = SharedBlob::encode(&mut encoder);

    // The safe head proves the first channel landed, receipt or not.
    assert_eq!(
        encoder.reconcile_derivation(BlockInfo::from(&shared.blocks[0]), None),
        DerivationReconciliation::Consistent
    );
    encoder.requeue(shared.packed.id);
    let retry = encoder.next_submission().expect("the shared blob is retried");
    assert_eq!(SubmissionFixture::frames(&retry), SubmissionFixture::frames(&shared.packed));
    encoder.confirm(retry.id, 10);

    let mut submissions = vec![shared.first, retry];
    submissions.extend(encoder.encode_and_drain().unwrap());
    for submission in &submissions[2..] {
        encoder.confirm(submission.id, 10);
    }
    assert_eq!(encoder.da_backlog_bytes(), 0);
    assert_eq!(fixture.derive(&submissions).concat(), BlockFixture::batches(&shared.blocks));
}
