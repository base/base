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

/// Retries go out in the order their data was built, across DA types. With the blob override
/// on, off and on again, the second blob is not packed with the first ahead of the calldata
/// built between them, which would land the channel frames out of order.
#[test]
fn retries_keep_their_order_across_da_types() {
    let config = EncoderConfig { da_type: DaType::Calldata, ..EncoderConfig::default() };
    let fixture = EncoderFixture::new(config);
    let mut encoder = fixture.encoder();
    let mut built = Vec::new();
    for (block, blob_override) in BlockFixture::chain(3, 0).into_iter().zip([true, false, true]) {
        encoder.set_blob_override(blob_override);
        encoder.add_block(block).unwrap();
        let [submission] = <[_; 1]>::try_from(encoder.encode_and_drain().unwrap()).unwrap();
        built.push(submission);
    }
    let da_types: Vec<_> = built.iter().map(BatchSubmission::da_type).collect();
    assert_eq!(da_types, [DaType::Blob, DaType::Calldata, DaType::Blob]);
    for submission in &built {
        encoder.requeue(submission.id);
    }

    for submission in &built {
        let retry = encoder.next_submission().expect("a retry");
        assert_eq!(retry.da_type(), submission.da_type());
        assert_eq!(SubmissionFixture::frames(&retry), SubmissionFixture::frames(submission));
    }
    assert!(encoder.next_submission().is_none());
}
