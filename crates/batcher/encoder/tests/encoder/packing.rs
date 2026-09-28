//! How `BatchEncoder` packs blobs into transactions, and the blob override.

use alloy_primitives::B256;
use base_batcher_encoder::{
    BatchPipeline, BatchSubmission, DaType, EncoderConfig, StepResult, SubmissionPayload,
};
use rstest::rstest;

use crate::common::{BlockFixture, EncoderFixture, SubmissionFixture, THREE_BLOB_PAYLOAD};

/// A transaction carries `max_blobs_per_tx` blobs while blobs are ready, and that cap cuts
/// transactions, not channels, so one channel spans several transactions.
#[rstest]
#[case::one_per_transaction(1, vec![1, 1, 1])]
#[case::two_per_transaction(2, vec![2, 1])]
fn transactions_carry_up_to_max_blobs_per_tx(
    #[case] max_blobs_per_tx: usize,
    #[case] blob_counts: Vec<usize>,
) {
    let config = EncoderConfig { max_blobs_per_tx, ..EncoderConfig::default() };
    let fixture = EncoderFixture::new(config);
    let mut encoder = fixture.encoder();
    encoder.add_block(BlockFixture::block(B256::ZERO, 1, THREE_BLOB_PAYLOAD)).unwrap();

    let submissions = encoder.encode_and_drain().unwrap();

    let counts: Vec<_> = submissions.iter().map(BatchSubmission::blob_count).collect();
    assert_eq!(counts, blob_counts);
    assert_eq!(fixture.derive(&submissions).len(), 1, "one channel");
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

    let blob_retry = encoder.next_submission().expect("the blob retry");
    assert_eq!(blob_retry.da_type(), DaType::Blob);
    assert_eq!(SubmissionFixture::frames(&blob_retry), SubmissionFixture::frames(&blob));
    let calldata_retry = encoder.next_submission().expect("the calldata retry");
    assert_eq!(calldata_retry.da_type(), DaType::Calldata);
    assert_eq!(SubmissionFixture::frames(&calldata_retry), SubmissionFixture::frames(&calldata));
}
