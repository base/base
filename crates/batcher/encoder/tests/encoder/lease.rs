//! The submission lease in [`BatchEncoder`]: ids, requeue, and what a reset forgets.

use alloy_primitives::B256;
use base_batcher_encoder::{BatchPipeline, DaType, EncoderConfig, StepResult};

use crate::common::{
    BlockFixture, EncoderFixture, MULTI_FRAME_PAYLOAD, SMALL_FRAME_SIZE, SubmissionFixture,
};

/// Requeued submissions come back with their own frames under new ids, in the order they were
/// produced, whatever the requeue order.
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
    let submission = encoder.encode_and_drain().unwrap().remove(0);
    assert!(submission.frame_count() > 1, "{} frames", submission.frame_count());
    encoder.add_block(blocks[1].clone()).unwrap();
    let newer = encoder.encode_and_drain().unwrap().remove(0);
    encoder.requeue(newer.id);

    encoder.requeue(submission.id);

    let retry = encoder.next_submission().expect("retry");
    assert_ne!(retry.id, submission.id);
    assert_eq!(SubmissionFixture::frames(&retry), SubmissionFixture::frames(&submission));
    assert_eq!(
        SubmissionFixture::frames(&encoder.next_submission().expect("newer output")),
        SubmissionFixture::frames(&newer)
    );
    assert!(encoder.next_submission().is_none());
}

/// Requeuing one submission does not resend the frames of another that was confirmed.
#[test]
fn requeue_does_not_resend_confirmed_frames() {
    let config = EncoderConfig {
        da_type: DaType::Calldata,
        max_frame_size: SMALL_FRAME_SIZE,
        ..EncoderConfig::default()
    };
    let fixture = EncoderFixture::new(config);
    let mut encoder = fixture.encoder();
    encoder.add_block(BlockFixture::block(B256::ZERO, 1, MULTI_FRAME_PAYLOAD)).unwrap();
    assert_eq!(encoder.step().unwrap(), StepResult::BlockEncoded);
    encoder.flush().unwrap();
    let first = encoder.next_submission().expect("first frame");
    let second = encoder.next_submission().expect("second frame");

    encoder.requeue(first.id);
    encoder.confirm(second.id, 1);

    let retry = encoder.next_submission().expect("retry");
    assert_eq!(SubmissionFixture::frames(&retry), SubmissionFixture::frames(&first));
    assert!(
        SubmissionFixture::drain(&mut encoder)
            .iter()
            .all(|submission| SubmissionFixture::frames(submission)
                != SubmissionFixture::frames(&second))
    );
}

/// A reset does not reuse ids: a confirmation or requeue for a submission issued before the
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

    encoder.confirm(stale.id, 42);
    assert!(encoder.da_backlog_bytes() > 0, "the fresh submission is not confirmed");
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
    let blocks = BlockFixture::chain(2, MULTI_FRAME_PAYLOAD);
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
