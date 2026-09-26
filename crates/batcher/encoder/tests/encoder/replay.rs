//! Confirmation-window replay in `BatchEncoder`. A channel whose frames cannot all land
//! within the derivation channel timeout is re-encoded under a fresh id, together with every
//! channel that shares a blob or an in-flight transaction with it and every later one.

use alloy_primitives::B256;
use base_batcher_encoder::{BatchPipeline, StepResult};
use rstest::rstest;

use crate::common::{
    BlockFixture, CHANNEL_TIMEOUT, EncoderFixture, MULTI_BLOB_PAYLOAD, MULTI_FRAME_PAYLOAD,
    SharedBlob, SharedRetry, SubmissionFixture,
};

/// A complete channel whose confirmations are more than the timeout apart, in either order, is
/// replayed under a fresh channel id, and confirming the replay clears the backlog.
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
}

/// A channel whose first frame landed more than the timeout ago while later frames are still
/// unconfirmed is replayed, and confirming the replay clears the backlog.
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

/// A channel is not replayed while derivation can still read it, whether all its frames
/// landed in one block, its confirmations are exactly the timeout apart, or it is incomplete
/// with the L1 head exactly at its deadline, where derivation still accepts a frame.
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

/// Replaying a channel also replays the channel whose tail shares a blob with its first
/// frames, even though that earlier channel was confirmed in time, because a blob lands whole.
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

/// Replaying a channel also replays the channel whose retried blob shares a transaction with
/// its retried tail, because a transaction lands whole. Confirming the replay clears the backlog.
#[test]
fn replay_includes_the_channel_sharing_a_transaction() {
    let fixture = EncoderFixture::one_channel_per_block(2);
    let mut encoder = fixture.encoder();
    let shared = SharedRetry::encode(&mut encoder);

    // The second channel's window closes with its tail, hence the retry, still in flight.
    encoder.confirm(shared.full_blobs.id, 1);
    encoder.advance_l1_head(1 + CHANNEL_TIMEOUT + 1);

    let replay = encoder.encode_and_drain().unwrap();
    assert_eq!(fixture.derive(&replay).concat(), BlockFixture::batches(&shared.blocks));
    for submission in &replay {
        encoder.confirm(submission.id, 5);
    }
    assert_eq!(encoder.da_backlog_bytes(), 0);
}

/// A transaction that landed confirms the channels it carried: replaying one of them does
/// not pull in the other, which is fully confirmed.
#[test]
fn a_landed_transaction_does_not_pull_its_other_channel_into_a_replay() {
    let fixture = EncoderFixture::one_channel_per_block(2);
    let mut encoder = fixture.encoder();
    let shared = SharedRetry::encode(&mut encoder);

    // The second channel's window closes with its full blobs still in flight.
    encoder.confirm(shared.retry.id, 1);
    encoder.advance_l1_head(1 + CHANNEL_TIMEOUT + 1);

    let replay = encoder.encode_and_drain().unwrap();
    assert_eq!(fixture.derive(&replay).concat(), BlockFixture::batches(&shared.blocks[1..]));
}

/// Replaying a channel discards every later channel too, but not the earlier ones that landed
/// in time, which stay confirmed. The submissions of the discarded channels are forgotten, so
/// requeuing them sends nothing.
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
    for submission in &replay {
        encoder.confirm(submission.id, 5);
    }
    assert_eq!(encoder.da_backlog_bytes(), 0, "the first channel is still confirmed");
}
