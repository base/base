//! Channel lifecycle in [`BatchEncoder`]: when a channel closes, when its partial output is
//! released, and what a flush does.

use alloy_primitives::B256;
use base_batcher_encoder::{BatchPipeline, DaEgress, EncoderConfig, StepResult, SubmissionPayload};
use base_protocol::Frame;
use rstest::rstest;

use crate::common::{BlockFixture, EncoderFixture, OpenChannel};

/// A channel opened at L1 head 0 closes when the head reaches
/// `max_channel_duration - sub_safety_margin`, and the close releases its output.
#[rstest]
#[case::with_margin(10, 4, 6)]
#[case::without_margin(5, 0, 5)]
fn channel_closes_at_its_effective_duration(
    #[case] max_channel_duration: u64,
    #[case] sub_safety_margin: u64,
    #[case] closes_at: u64,
) {
    let config =
        EncoderConfig { max_channel_duration, sub_safety_margin, ..EncoderConfig::default() };
    let fixture = EncoderFixture::new(config);
    let mut encoder = fixture.encoder();
    encoder.add_block(BlockFixture::block(B256::ZERO, 1, 0)).unwrap();
    assert_eq!(encoder.step().unwrap(), StepResult::BlockEncoded);

    encoder.advance_l1_head(closes_at - 1);
    assert!(encoder.next_submission().is_none(), "the channel is still open");

    encoder.advance_l1_head(closes_at);
    assert!(encoder.next_submission().is_some(), "the close releases the channel");
}

/// An L1 head below the current one is ignored: a channel opened afterwards still measures
/// its duration from the highest head seen.
#[test]
fn l1_head_never_moves_backwards() {
    let fixture =
        EncoderFixture::new(EncoderConfig { max_channel_duration: 2, ..EncoderConfig::default() });
    let mut encoder = fixture.encoder();
    let blocks = BlockFixture::chain(2, 0);
    encoder.advance_l1_head(10);
    encoder.add_block(blocks[0].clone()).unwrap();
    encoder.encode_and_drain().unwrap();

    encoder.advance_l1_head(3);
    encoder.add_block(blocks[1].clone()).unwrap();
    assert_eq!(encoder.step().unwrap(), StepResult::BlockEncoded);

    encoder.advance_l1_head(11);
    assert!(encoder.next_submission().is_none(), "the channel opened at head 10 is due at 12");
    encoder.advance_l1_head(12);
    assert!(encoder.next_submission().is_some());
}

/// The partial output of a channel closed on its size target is held back, since more data may
/// fill the blob, until a flush releases it.
#[test]
fn flush_releases_the_tail_of_a_size_closed_channel() {
    let config = EncoderConfig { compressed_size_target: Some(1), ..EncoderConfig::default() };
    let fixture = EncoderFixture::new(config);
    let mut encoder = fixture.encoder();
    encoder.add_block(BlockFixture::block(B256::ZERO, 1, 0)).unwrap();
    assert_eq!(encoder.step().unwrap(), StepResult::ChannelClosed);
    assert!(encoder.next_submission().is_none());

    encoder.flush().unwrap();

    assert!(encoder.next_submission().is_some());
}

/// A held tail is released at the deadline of its own channel, not at that of a later one.
#[test]
fn size_closed_tail_keeps_its_original_deadline() {
    let config = EncoderConfig {
        compressed_size_target: Some(1),
        max_channel_duration: 5,
        ..EncoderConfig::default()
    };
    let fixture = EncoderFixture::new(config);
    let mut encoder = fixture.encoder();
    let blocks = BlockFixture::chain(2, 0);
    for block in &blocks {
        encoder.add_block(block.clone()).unwrap();
    }

    // The first channel opens at head 10 and is due at 15, the second at 12 and 17.
    encoder.advance_l1_head(10);
    assert_eq!(encoder.step().unwrap(), StepResult::ChannelClosed);
    encoder.advance_l1_head(12);
    assert_eq!(encoder.step().unwrap(), StepResult::ChannelClosed);

    encoder.advance_l1_head(14);
    assert!(encoder.next_submission().is_none(), "no tail is due yet");

    encoder.advance_l1_head(15);
    assert!(encoder.next_submission().is_some(), "the first tail is due");
}

/// An open channel emits every blob it fills without closing: the channel takes more blocks
/// after the blob went out, and the frames emitted early and the rest decode as one channel.
#[test]
fn open_channel_emits_full_blobs_without_closing() {
    let fixture = EncoderFixture::new(EncoderConfig::default());
    let mut encoder = fixture.encoder();
    let mut open = OpenChannel::encode(&mut encoder);
    let SubmissionPayload::Blobs(blobs) = open.first.payload() else {
        panic!("expected a blob submission");
    };
    for blob in blobs {
        assert!(blob.frames().iter().all(|frame| !frame.is_last), "the channel is still open");
        assert!(
            DaEgress::BLOB_CAPACITY - blob.frame_bytes() <= Frame::ENCODED_OVERHEAD,
            "the blob has room for another frame"
        );
    }

    open.add_next_block(&mut encoder);
    let batches = open.batches();

    let mut submissions = vec![open.first];
    submissions.extend(encoder.encode_and_drain().unwrap());
    let derived = fixture.derive(&submissions);
    assert_eq!(derived.len(), 1, "the early blob and the rest are one channel");
    assert_eq!(derived.concat(), batches);
}

/// Nothing queued, nothing to submit.
#[test]
fn encode_and_drain_without_blocks_returns_nothing() {
    let fixture = EncoderFixture::new(EncoderConfig::default());
    let mut encoder = fixture.encoder();

    assert!(encoder.encode_and_drain().unwrap().is_empty());
}
