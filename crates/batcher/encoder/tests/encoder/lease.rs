//! The submission lease in `BatchEncoder`, its ids, its requeue, and what a reset forgets.

use alloy_primitives::B256;
use base_batcher_encoder::{BatchPipeline, DaType, EncoderConfig, StepResult};

use crate::common::{
    BlockFixture, EncoderFixture, MULTI_FRAME_PAYLOAD, SMALL_FRAME_SIZE, SubmissionFixture,
};

/// Two submissions requeued newest first come back oldest first, each with exactly its own
/// frames, the older one under a new id.
#[test]
fn requeued_submissions_resend_their_frames_in_production_order() {
    let config = EncoderConfig {
        max_frame_size: SMALL_FRAME_SIZE,
        max_blobs_per_tx: 1,
        ..EncoderConfig::default()
    };
    let fixture = EncoderFixture::new(config);
    let mut encoder = fixture.encoder();
    let blocks = BlockFixture::chain(2, MULTI_FRAME_PAYLOAD);
    encoder.add_block(blocks[0].clone()).unwrap();
    let [older] = <[_; 1]>::try_from(encoder.encode_and_drain().unwrap()).unwrap();
    assert!(older.frame_count() > 1, "{} frames", older.frame_count());
    encoder.add_block(blocks[1].clone()).unwrap();
    let [newer] = <[_; 1]>::try_from(encoder.encode_and_drain().unwrap()).unwrap();

    encoder.requeue(newer.id);
    encoder.requeue(older.id);

    let retry = encoder.next_submission().expect("retry");
    assert_ne!(retry.id, older.id);
    assert_eq!(SubmissionFixture::frames(&retry), SubmissionFixture::frames(&older));
    assert_eq!(
        SubmissionFixture::frames(&encoder.next_submission().expect("newer output")),
        SubmissionFixture::frames(&newer)
    );
    assert!(encoder.next_submission().is_none());
}

/// A retry goes out as the transaction that failed, never packed with blobs built after its
/// requeue.
#[test]
fn a_retry_is_not_packed_with_newer_output() {
    let config = EncoderConfig { max_blobs_per_tx: 2, ..EncoderConfig::default() };
    let fixture = EncoderFixture::new(config);
    let mut encoder = fixture.encoder();
    let blocks = BlockFixture::chain(2, MULTI_FRAME_PAYLOAD);
    encoder.add_block(blocks[0].clone()).unwrap();
    let submission = encoder.encode_and_drain().unwrap().remove(0);
    encoder.requeue(submission.id);
    encoder.add_block(blocks[1].clone()).unwrap();
    assert_eq!(encoder.step().unwrap(), StepResult::BlockEncoded);
    encoder.flush().unwrap();

    let retry = encoder.next_submission().expect("the retry");
    assert_eq!(SubmissionFixture::frames(&retry), SubmissionFixture::frames(&submission));
    let newer = encoder.next_submission().expect("the newer output");
    assert!(encoder.next_submission().is_none());
    assert_eq!(fixture.derive(&[retry, newer]).concat(), BlockFixture::batches(&blocks));
}

/// Requeuing one submission resends exactly its frames, and none from the other submissions of
/// its channel still in flight.
#[test]
fn requeue_resends_only_that_submission() {
    let config = EncoderConfig {
        da_type: DaType::Calldata,
        max_frame_size: SMALL_FRAME_SIZE,
        ..EncoderConfig::default()
    };
    let fixture = EncoderFixture::new(config);
    let mut encoder = fixture.encoder();
    encoder.add_block(BlockFixture::block(B256::ZERO, 1, MULTI_FRAME_PAYLOAD)).unwrap();
    let submissions = encoder.encode_and_drain().unwrap();
    assert!(submissions.len() > 1, "{} submissions", submissions.len());

    encoder.requeue(submissions[0].id);

    let retry = encoder.next_submission().expect("retry");
    assert_eq!(SubmissionFixture::frames(&retry), SubmissionFixture::frames(&submissions[0]));
    assert!(encoder.next_submission().is_none(), "no other submission is resent");
}

/// A reset does not reuse ids, so a confirmation or requeue for a submission issued before the
/// reset leaves the submissions issued after it alone.
#[test]
fn stale_ids_do_not_touch_submissions_issued_after_a_reset() {
    let fixture = EncoderFixture::new(EncoderConfig::default());
    let mut encoder = fixture.encoder();
    encoder.add_block(BlockFixture::block(B256::ZERO, 1, 0)).unwrap();
    assert_eq!(encoder.step().unwrap(), StepResult::BlockEncoded);
    encoder.flush().unwrap();
    let stale = encoder.next_submission().expect("submission before the reset");

    encoder.reset();
    encoder.add_block(BlockFixture::block(B256::ZERO, 1, 0)).unwrap();
    assert_eq!(encoder.step().unwrap(), StepResult::BlockEncoded);
    encoder.flush().unwrap();
    let fresh = encoder.next_submission().expect("submission after the reset");
    assert_ne!(fresh.id, stale.id);

    let backlog = encoder.da_backlog_bytes();
    encoder.confirm(stale.id, 42);
    assert_eq!(encoder.da_backlog_bytes(), backlog, "the fresh submission is not confirmed");
    encoder.requeue(stale.id);
    assert!(encoder.next_submission().is_none(), "nothing is resent");

    encoder.confirm(fresh.id, 43);
    assert_eq!(encoder.da_backlog_bytes(), 0);
}

/// A reset drops every buffered block and channel, and the next block is accepted whatever
/// its parent.
#[test]
fn reset_drops_buffered_state_and_accepts_any_parent() {
    let fixture = EncoderFixture::new(EncoderConfig::default());
    let mut encoder = fixture.encoder();
    let blocks = BlockFixture::chain(2, 0);
    for block in &blocks {
        encoder.add_block(block.clone()).unwrap();
    }
    assert_eq!(encoder.step().unwrap(), StepResult::BlockEncoded);

    encoder.reset();

    assert_eq!(encoder.step().unwrap(), StepResult::Idle);
    encoder.flush().unwrap();
    assert!(encoder.next_submission().is_none());
    assert_eq!(encoder.da_backlog_bytes(), 0);
    encoder.add_block(BlockFixture::block(B256::repeat_byte(0xab), 7, 0)).unwrap();
}
