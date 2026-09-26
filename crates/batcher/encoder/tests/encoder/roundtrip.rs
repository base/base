//! Round trip through the flush path: derivation reads back the batches of the blocks the
//! encoder was given, in order, through six wire shapes: both DA types, small and full
//! frames, a channel over several blobs and transactions, several channels in one blob.

use base_batcher_encoder::{BatchPipeline, BatchSubmission, DaType, EncoderConfig};
use rstest::rstest;

use crate::common::{BlockFixture, EncoderFixture};

/// The number of blocks every case encodes.
const BLOCK_COUNT: u64 = 4;

/// Every block encoded comes back as its batch, in order, and the submissions have the wire
/// shape the configuration asks for: `blob_count` blobs over all of them, `channel_count`
/// channels read back.
#[rstest]
#[case::blobs(EncoderConfig::default(), 1_000, 1, 1)]
#[case::calldata(
    EncoderConfig { da_type: DaType::Calldata, ..EncoderConfig::default() },
    1_000,
    0,
    1
)]
#[case::calldata_small_frames(
    EncoderConfig { da_type: DaType::Calldata, max_frame_size: 64, ..EncoderConfig::default() },
    1_000,
    0,
    1
)]
#[case::blobs_small_frames(
    EncoderConfig { max_frame_size: 64, ..EncoderConfig::default() },
    1_000,
    1,
    1
)]
#[case::channel_across_blobs_and_transactions(
    EncoderConfig { max_blobs_per_tx: 2, ..EncoderConfig::default() },
    200_000,
    7,
    1
)]
// A target of one byte closes every channel on its first block.
#[case::channels_packed_in_one_blob(
    EncoderConfig { compressed_size_target: Some(1), ..EncoderConfig::default() },
    1_000,
    1,
    BLOCK_COUNT as usize
)]
fn derivation_reads_back_the_encoded_blocks(
    #[case] config: EncoderConfig,
    #[case] payload_len: usize,
    #[case] blob_count: usize,
    #[case] channel_count: usize,
) {
    let fixture = EncoderFixture::new(config);
    let mut encoder = fixture.encoder();
    let blocks = BlockFixture::chain(BLOCK_COUNT, payload_len);
    for block in &blocks {
        encoder.add_block(block.clone()).expect("fixture blocks should chain from genesis");
    }

    let submissions = encoder.encode_and_drain().expect("blocks should encode");
    assert_eq!(submissions.iter().map(BatchSubmission::blob_count).sum::<usize>(), blob_count);

    let derived = fixture.derive(&submissions);
    assert_eq!(derived.len(), channel_count);
    assert_eq!(derived.concat(), BlockFixture::batches(&blocks));
}
