//! Confirmation-window replay in [`BatchEncoder`]: a channel whose frames cannot all land
//! within the derivation channel timeout is re-encoded under a fresh id, together with every
//! channel that shares a blob or a transaction with it and every later one.

use alloy_primitives::B256;
use base_batcher_encoder::{BatchPipeline, DerivationReconciliation, StepResult};
use base_protocol::BlockInfo;
use rstest::rstest;

use crate::common::{
    BlockFixture, CHANNEL_TIMEOUT, EncoderFixture, MULTI_BLOB_PAYLOAD, MULTI_FRAME_PAYLOAD,
    SharedBlob, SubmissionFixture, THREE_BLOB_PAYLOAD,
};

/// A complete channel whose confirmations are more than the timeout apart, in either order,
/// is replayed: the block is re-encoded under a fresh channel id.
#[rstest]
#[case::ascending(1, 4)]
#[case::descending(100, 90)]
fn confirmations_spanning_more_than_the_timeout_replay_the_channel(
    #[case] first_l1_block: u64,
    #[case] other_l1_block: u64,
) {
    let fixture = EncoderFixture::small_calldata_frames();
    let mut encoder = fixture.encoder();
    let block = BlockFixture::block(B256::ZERO, 1, MULTI_FRAME_PAYLOAD);
    encoder.add_block(block.clone()).unwrap();
    let submissions = encoder.encode_and_drain().unwrap();
    assert!(submissions.len() > 1, "{} submissions", submissions.len());

    encoder.confirm(submissions[0].id, first_l1_block);
    encoder.advance_l1_head(first_l1_block);
    for submission in &submissions[1..] {
        encoder.confirm(submission.id, other_l1_block);
    }
    encoder.advance_l1_head(other_l1_block);

    assert_eq!(encoder.step().unwrap(), StepResult::BlockEncoded, "the block is re-encoded");
    let replay = encoder.encode_and_drain().unwrap();
    assert_eq!(
        fixture.derive(&replay).concat(),
        BlockFixture::batches(std::slice::from_ref(&block))
    );
    assert_ne!(
        SubmissionFixture::frames(&replay[0])[0].id,
        SubmissionFixture::frames(&submissions[0])[0].id,
        "a fresh channel"
    );

    for submission in &replay {
        encoder.confirm(submission.id, other_l1_block + 1);
    }
    assert_eq!(encoder.da_backlog_bytes(), 0);
    assert_eq!(
        encoder.reconcile_derivation(BlockInfo::from(&block), None),
        DerivationReconciliation::Consistent
    );
}

/// A channel whose first frame landed more than the timeout ago while later frames are still
/// unconfirmed is replayed, and the backlog follows the replay.
#[test]
fn expiry_before_the_last_frame_lands_replays_the_channel() {
    let fixture = EncoderFixture::small_calldata_frames();
    let mut encoder = fixture.encoder();
    let block = BlockFixture::block(B256::ZERO, 1, MULTI_FRAME_PAYLOAD);
    encoder.add_block(block.clone()).unwrap();
    let submissions = encoder.encode_and_drain().unwrap();
    assert!(submissions.len() > 1, "{} submissions", submissions.len());

    encoder.confirm(submissions[0].id, 1);
    encoder.advance_l1_head(1 + CHANNEL_TIMEOUT + 1);

    assert_eq!(encoder.step().unwrap(), StepResult::BlockEncoded, "the block is re-encoded");
    let replay = encoder.encode_and_drain().unwrap();
    assert_eq!(fixture.derive(&replay).concat(), BlockFixture::batches(&[block]));

    for submission in &replay {
        encoder.confirm(submission.id, 5);
    }
    assert_eq!(encoder.da_backlog_bytes(), 0);
}

/// A channel within its window is never replayed, however far the L1 head moves: all frames
/// confirmed in one block, confirmations exactly the timeout apart, or an incomplete channel
/// exactly at its deadline, since derivation still accepts a frame there.
#[rstest]
#[case::all_in_one_block(Some(1), 100)]
#[case::span_of_the_timeout(Some(1 + CHANNEL_TIMEOUT), 1 + CHANNEL_TIMEOUT)]
#[case::incomplete_at_the_deadline(None, 1 + CHANNEL_TIMEOUT)]
fn channel_within_its_window_is_not_replayed(
    #[case] rest_l1_block: Option<u64>,
    #[case] l1_head: u64,
) {
    let fixture = EncoderFixture::small_calldata_frames();
    let mut encoder = fixture.encoder();
    encoder.add_block(BlockFixture::block(B256::ZERO, 1, MULTI_FRAME_PAYLOAD)).unwrap();
    let submissions = encoder.encode_and_drain().unwrap();
    assert!(submissions.len() > 1, "{} submissions", submissions.len());

    encoder.confirm(submissions[0].id, 1);
    if let Some(rest_l1_block) = rest_l1_block {
        for submission in &submissions[1..] {
            encoder.confirm(submission.id, rest_l1_block);
        }
    }
    encoder.advance_l1_head(l1_head);

    assert_eq!(encoder.step().unwrap(), StepResult::Idle);
    assert!(encoder.next_submission().is_none());
}

/// Blobs are atomic: replaying a channel replays the channel whose tail shares a blob with
/// its first frames, even though that earlier channel was confirmed in time.
#[test]
fn replay_includes_the_channel_sharing_a_blob() {
    let fixture = EncoderFixture::one_channel_per_block(1);
    let mut encoder = fixture.encoder();
    let shared = SharedBlob::encode(&mut encoder);

    // The first channel lands in time. The second one still has its tail held back when its
    // window closes.
    encoder.confirm(shared.first.id, 1);
    encoder.confirm(shared.packed.id, 1);
    encoder.advance_l1_head(1 + CHANNEL_TIMEOUT + 1);

    assert_eq!(encoder.step().unwrap(), StepResult::ChannelClosed, "the first block is re-encoded");
    assert_eq!(
        encoder.step().unwrap(),
        StepResult::ChannelClosed,
        "the second block is re-encoded"
    );
    let replay = encoder.encode_and_drain().unwrap();
    assert_eq!(fixture.derive(&replay).concat(), BlockFixture::batches(&shared.blocks));
}

/// Transactions are atomic too: replaying a channel replays the channel whose retried blob
/// shares a transaction with its retried tail.
#[test]
fn replay_includes_the_channel_sharing_a_transaction() {
    let fixture = EncoderFixture::one_channel_per_block(2);
    let mut encoder = fixture.encoder();
    let first_block = BlockFixture::block(B256::ZERO, 1, 0);
    let second_block = BlockFixture::block(first_block.header.hash_slow(), 2, THREE_BLOB_PAYLOAD);
    encoder.add_block(first_block.clone()).unwrap();
    let first = encoder.encode_and_drain().unwrap();
    encoder.add_block(second_block.clone()).unwrap();
    let second = encoder.encode_and_drain().unwrap();
    assert_eq!(second.len(), 2, "two full blobs, then the tail");

    // The first channel's blob and the second channel's tail are retried in one transaction.
    encoder.confirm(second[0].id, 1);
    encoder.requeue(first[0].id);
    encoder.requeue(second[1].id);
    let retry = encoder.next_submission().expect("the retry");
    assert_eq!(retry.blob_count(), 2, "one transaction carries both channels");
    encoder.advance_l1_head(1 + CHANNEL_TIMEOUT + 1);

    let replay = encoder.encode_and_drain().unwrap();
    assert_eq!(
        fixture.derive(&replay).concat(),
        BlockFixture::batches(&[first_block, second_block])
    );
    for submission in &replay {
        encoder.confirm(submission.id, 5);
    }
    assert_eq!(encoder.da_backlog_bytes(), 0);
}

/// Replaying a channel discards every later channel too, but not the earlier ones that landed
/// in time, and the submissions of the discarded channels are forgotten: requeuing them sends
/// nothing.
#[test]
fn replay_discards_every_later_channel() {
    let fixture = EncoderFixture::one_channel_per_block(1);
    let mut encoder = fixture.encoder();
    let blocks = BlockFixture::chain(3, MULTI_BLOB_PAYLOAD);

    // The first channel lands in time.
    encoder.add_block(blocks[0].clone()).unwrap();
    for submission in encoder.encode_and_drain().unwrap() {
        encoder.confirm(submission.id, 1);
    }

    // The second channel has its tail in flight when its window closes, the third is in flight.
    encoder.add_block(blocks[1].clone()).unwrap();
    let second = encoder.encode_and_drain().unwrap();
    assert!(second.len() > 1, "{} submissions", second.len());
    encoder.confirm(second[0].id, 1);
    encoder.add_block(blocks[2].clone()).unwrap();
    let third = encoder.encode_and_drain().unwrap();
    encoder.advance_l1_head(1 + CHANNEL_TIMEOUT + 1);

    let replay = encoder.encode_and_drain().unwrap();
    assert_eq!(fixture.derive(&replay).concat(), BlockFixture::batches(&blocks[1..]));
    for submission in second[1..].iter().chain(&third) {
        encoder.requeue(submission.id);
    }
    assert!(
        SubmissionFixture::drain(&mut encoder).is_empty(),
        "the discarded submissions are forgotten"
    );
}
