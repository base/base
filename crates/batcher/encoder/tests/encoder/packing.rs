//! How `BatchEncoder` packs blobs into transactions, and the blob override.

use alloy_primitives::B256;
use base_batcher_encoder::{BatchPipeline, DaType, EncoderConfig, StepResult};
use rstest::rstest;

use crate::common::{BlockFixture, EncoderFixture, SubmissionFixture, THREE_BLOB_PAYLOAD};

/// A transaction carries `max_blobs_per_tx` blobs while blobs are ready, and that cap cuts
/// transactions, not channels, so one channel spans several transactions.
#[rstest]
#[case(1)]
#[case(2)]
fn transactions_carry_up_to_max_blobs_per_tx(#[case] max_blobs_per_tx: usize) {
    let config = EncoderConfig { max_blobs_per_tx, ..EncoderConfig::default() };
    let fixture = EncoderFixture::new(config);
    let mut encoder = fixture.encoder();
    encoder.add_block(BlockFixture::block(B256::ZERO, 1, THREE_BLOB_PAYLOAD)).unwrap();

    let submissions = encoder.encode_and_drain().unwrap();

    assert!(submissions.len() >= 2, "{} submissions", submissions.len());
    assert_eq!(submissions[0].blob_count(), max_blobs_per_tx);
    assert_eq!(fixture.derive(&submissions).len(), 1);
}

/// While the blob override is active, a calldata encoder emits blobs, and a retry keeps the
/// DA type its submission was built with.
#[test]
fn blob_override_switches_a_calldata_encoder_to_blobs() {
    let config = EncoderConfig { da_type: DaType::Calldata, ..EncoderConfig::default() };
    let fixture = EncoderFixture::new(config);
    let mut encoder = fixture.encoder();
    encoder.add_block(BlockFixture::block(B256::ZERO, 1, 0)).unwrap();
    assert_eq!(encoder.step().unwrap(), StepResult::BlockEncoded);
    encoder.flush().unwrap();

    encoder.set_blob_override(true);
    let submission = encoder.next_submission().expect("submission under the override");
    assert_eq!(submission.da_type(), DaType::Blob);

    encoder.requeue(submission.id);
    encoder.set_blob_override(false);
    let retry = encoder.next_submission().expect("retry after the override");
    assert_eq!(retry.da_type(), DaType::Blob);
}

/// A blob built under the override is retried on its own, never packed with the calldata
/// built once the override is off, since a transaction carries one DA type.
#[test]
fn a_blob_retry_is_not_packed_with_calldata() {
    let config = EncoderConfig { da_type: DaType::Calldata, ..EncoderConfig::default() };
    let fixture = EncoderFixture::new(config);
    let mut encoder = fixture.encoder();
    let blocks = BlockFixture::chain(2, 0);
    encoder.set_blob_override(true);
    encoder.add_block(blocks[0].clone()).unwrap();
    let blob = encoder.encode_and_drain().unwrap().remove(0);
    encoder.set_blob_override(false);
    encoder.add_block(blocks[1].clone()).unwrap();
    let calldata = encoder.encode_and_drain().unwrap().remove(0);

    encoder.requeue(blob.id);
    encoder.requeue(calldata.id);

    let retry = encoder.next_submission().expect("the blob retry");
    assert_eq!(retry.da_type(), DaType::Blob);
    assert_eq!(SubmissionFixture::frames(&retry), SubmissionFixture::frames(&blob));
    let retry = encoder.next_submission().expect("the calldata retry");
    assert_eq!(retry.da_type(), DaType::Calldata);
    assert_eq!(SubmissionFixture::frames(&retry), SubmissionFixture::frames(&calldata));
}
