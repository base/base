//! Construction and block intake of [`BatchEncoder`]: the config check, the parent check, one
//! transition per step, the retry of a block the open channel cannot take, and the fatal
//! errors that stop the batcher rather than skip a block.

use std::sync::Arc;

use alloy_primitives::B256;
use base_batcher_encoder::{
    BatchComposeError, BatchEncoder, BatchPipeline, Channel, ChannelLimit, EncoderConfig,
    EncoderConfigError, ReorgError, StepError, StepResult,
};
use base_common_consensus::BaseBlock;
use base_common_genesis::RollupConfig;
use base_protocol::Frame;
use rstest::rstest;

use crate::common::{BlockFixture, EncoderFixture};

/// The encoder validates its config on construction.
#[test]
fn new_rejects_an_invalid_config() {
    let config = EncoderConfig { max_blobs_per_tx: 0, ..EncoderConfig::default() };

    assert!(matches!(
        BatchEncoder::new(Arc::new(RollupConfig::default()), config),
        Err(EncoderConfigError::MaxBlobsPerTxZero)
    ));
}

/// A block whose parent is not the last accepted block is handed back with the hash the
/// encoder expected, and the chain goes on from that block.
#[test]
fn add_block_rejects_a_block_off_the_buffered_chain() {
    let fixture = EncoderFixture::new(EncoderConfig::default());
    let mut encoder = fixture.encoder();
    let blocks = BlockFixture::chain(2, 0);
    encoder.add_block(blocks[0].clone()).unwrap();

    let stray = BlockFixture::block(B256::repeat_byte(0xab), 2, 0);
    let (error, returned) = encoder.add_block(stray.clone()).unwrap_err();

    let ReorgError::ParentMismatch { expected, got } = error;
    assert_eq!(expected, blocks[0].header.hash_slow());
    assert_eq!(got, stray.header.parent_hash);
    assert_eq!(*returned, stray);
    encoder.add_block(blocks[1].clone()).unwrap();
}

/// A block that does not compose into a batch is fatal, and it stays queued: the next step
/// fails the same way instead of skipping it, which would leave a gap in the L2 chain on L1.
#[rstest]
#[case::no_transactions(BlockFixture::without_transactions, BatchComposeError::EmptyBlock)]
#[case::no_deposit_first(BlockFixture::without_deposit, BatchComposeError::NotDepositTx)]
#[case::undecodable_l1_info(
    BlockFixture::with_undecodable_l1_info,
    BatchComposeError::L1InfoDecode
)]
fn step_fails_on_a_block_that_does_not_compose(
    #[case] spoil: fn(BaseBlock) -> BaseBlock,
    #[case] expected: BatchComposeError,
) {
    let fixture = EncoderFixture::new(EncoderConfig::default());
    let mut encoder = fixture.encoder();
    encoder.add_block(spoil(BlockFixture::block(B256::ZERO, 1, 0))).unwrap();

    for _ in 0..2 {
        let error = encoder.step().unwrap_err();
        assert!(
            matches!(&error, StepError::CompositionFailed { cursor: 0, source } if *source == expected),
            "{error}"
        );
    }
}

/// Frames of one data byte: a channel then carries at most `Channel::MAX_FRAMES` bytes.
const ONE_BYTE_FRAMES: usize = Frame::ENCODED_OVERHEAD + 1;

/// A batch the open channel cannot take closes that channel on its protocol limit, and the
/// block is retried in a new one. Derivation reads both blocks, in order.
#[test]
fn step_retries_a_rejected_block_in_a_new_channel() {
    let config = EncoderConfig { max_frame_size: ONE_BYTE_FRAMES, ..EncoderConfig::default() };
    let fixture = EncoderFixture::new(config);
    let mut encoder = fixture.encoder();
    // One block fits a channel, two do not.
    let blocks = BlockFixture::chain(2, Channel::MAX_FRAMES * 2 / 3);
    for block in &blocks {
        encoder.add_block(block.clone()).unwrap();
    }

    assert_eq!(encoder.step().unwrap(), StepResult::BlockEncoded);
    assert_eq!(encoder.step().unwrap(), StepResult::ChannelClosed);
    assert_eq!(encoder.step().unwrap(), StepResult::BlockEncoded);
    assert_eq!(encoder.step().unwrap(), StepResult::Idle);

    let submissions = encoder.encode_and_drain().unwrap();
    let derived = fixture.derive(&submissions);
    assert_eq!(derived.len(), 2);
    assert_eq!(derived.concat(), BlockFixture::batches(&blocks));
}

/// A block that does not fit an empty channel is fatal, since no channel could carry it. It is
/// not skipped, and nothing is emitted for it.
#[test]
fn step_fails_on_a_block_no_channel_can_carry() {
    let config = EncoderConfig { max_frame_size: ONE_BYTE_FRAMES, ..EncoderConfig::default() };
    let fixture = EncoderFixture::new(config);
    let mut encoder = fixture.encoder();
    let block = BlockFixture::block(B256::ZERO, 1, Channel::MAX_FRAMES * 3 / 2);
    encoder.add_block(block).unwrap();

    for _ in 0..2 {
        let error = encoder.step().unwrap_err();
        assert!(
            matches!(
                error,
                StepError::BlockExceedsChannelLimit {
                    cursor: 0,
                    limit: ChannelLimit::FrameCount { .. }
                }
            ),
            "{error}"
        );
    }
    encoder.flush().unwrap();
    assert!(encoder.next_submission().is_none());
}
