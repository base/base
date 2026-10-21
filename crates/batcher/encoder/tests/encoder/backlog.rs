//! The DA backlog reported by `BatchEncoder`.

use alloy_primitives::B256;
use base_batcher_encoder::{BatchPipeline, BufferedBlock, EncoderConfig, StepResult};

use crate::common::{BlockFixture, EncoderFixture, OpenChannel, SubmissionFixture};

/// The backlog is the estimated DA size of a block from the moment it is queued until its
/// channel is fully confirmed.
#[test]
fn backlog_counts_the_estimated_da_bytes_until_confirmation() {
    let fixture = EncoderFixture::new(EncoderConfig::default());
    let mut encoder = fixture.encoder();
    let block = BlockFixture::block(B256::ZERO, 1, 0);
    let block_da_bytes = BufferedBlock::new(block.clone()).estimated_da_bytes();
    encoder.add_block(block).unwrap();
    assert_eq!(encoder.da_backlog_bytes(), block_da_bytes);

    assert_eq!(encoder.step().unwrap(), StepResult::BlockEncoded);
    assert_eq!(encoder.da_backlog_bytes(), block_da_bytes, "encoding changes nothing");

    let submissions = encoder.encode_and_drain().unwrap();
    assert_eq!(encoder.da_backlog_bytes(), block_da_bytes, "in flight changes nothing");

    for submission in submissions {
        encoder.confirm(submission.id, 1);
    }
    assert_eq!(encoder.da_backlog_bytes(), 0);
}

/// A channel stays in the backlog until its last frame is confirmed, so confirming every blob
/// an open channel emitted so far changes nothing.
#[test]
fn backlog_stays_until_the_last_frame_is_confirmed() {
    let fixture = EncoderFixture::new(EncoderConfig::default());
    let mut encoder = fixture.encoder();
    let open = OpenChannel::encode(&mut encoder);
    let mut emitted = vec![open.first];
    emitted.extend(SubmissionFixture::drain(&mut encoder));
    let backlog = encoder.da_backlog_bytes();
    for submission in &emitted {
        encoder.confirm(submission.id, 1);
    }
    assert_eq!(encoder.da_backlog_bytes(), backlog, "the channel is still open");

    for submission in encoder.encode_and_drain().unwrap() {
        encoder.confirm(submission.id, 1);
    }
    assert_eq!(encoder.da_backlog_bytes(), 0);
}
