//! The blob override of `BatchEncoder`.

use alloy_primitives::B256;
use base_batcher_encoder::{BatchPipeline, DaType, EncoderConfig, StepResult};

use crate::common::{BlockFixture, EncoderFixture};

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
