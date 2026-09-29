//! How `BatchEncoder` packs blobs into transactions, and the blob override.

use alloy_primitives::B256;
use base_batcher_encoder::{BatchPipeline, BatchSubmission, DaType, EncoderConfig};

use crate::common::{BlockFixture, EncoderFixture, SubmissionFixture, THREE_BLOB_PAYLOAD};

/// A transaction carries `max_blobs_per_tx` blobs while blobs are ready.
#[test]
fn transactions_carry_up_to_max_blobs_per_tx() {
    let config = EncoderConfig { max_blobs_per_tx: 2, ..EncoderConfig::default() };
    let fixture = EncoderFixture::new(config);
    let mut encoder = fixture.encoder();
    encoder.add_block(BlockFixture::block(B256::ZERO, 1, THREE_BLOB_PAYLOAD)).unwrap();

    let submissions = encoder.encode_and_drain().unwrap();

    let counts: Vec<_> = submissions.iter().map(BatchSubmission::blob_count).collect();
    assert_eq!(counts, [2, 1]);
}

/// A calldata encoder emits blobs while the blob override is active. A blob built under the
/// override is retried as a blob once it is off, on its own, never packed with the calldata
/// built since, because a transaction carries one DA type.
#[test]
fn a_blob_retry_is_not_packed_with_calldata() {
    let config = EncoderConfig { da_type: DaType::Calldata, ..EncoderConfig::default() };
    let fixture = EncoderFixture::new(config);
    let mut encoder = fixture.encoder();
    let blocks = BlockFixture::chain(2, 0);
    encoder.set_blob_override(true);
    encoder.add_block(blocks[0].clone()).unwrap();
    let blob = encoder.encode_and_drain().unwrap().remove(0);
    assert_eq!(blob.da_type(), DaType::Blob);
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
