//! The DA backlog reported by [`BatchEncoder`].

use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::B256;
use base_batcher_encoder::{BatchPipeline, EncoderConfig, StepResult};

use crate::common::{BlockFixture, EncoderFixture, MULTI_FRAME_PAYLOAD};

/// The backlog is the bytes of the user transactions, deposits excluded, from the moment a
/// block is queued until its channel is fully confirmed.
#[test]
fn backlog_counts_user_transaction_bytes_until_confirmation() {
    let fixture = EncoderFixture::new(EncoderConfig::default());
    let mut encoder = fixture.encoder();
    let block = BlockFixture::block(B256::ZERO, 1, MULTI_FRAME_PAYLOAD);
    let user_tx_bytes = block.body.transactions[1].encode_2718_len() as u64;
    encoder.add_block(block).unwrap();
    assert_eq!(encoder.da_backlog_bytes(), user_tx_bytes);

    assert_eq!(encoder.step().unwrap(), StepResult::BlockEncoded);
    assert_eq!(encoder.da_backlog_bytes(), user_tx_bytes, "encoding changes nothing");

    let submissions = encoder.encode_and_drain().unwrap();
    assert_eq!(encoder.da_backlog_bytes(), user_tx_bytes, "in flight changes nothing");

    for submission in submissions {
        encoder.confirm(submission.id, 1);
    }
    assert_eq!(encoder.da_backlog_bytes(), 0);
}
