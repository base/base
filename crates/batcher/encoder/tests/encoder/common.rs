//! Fixtures shared by the encoder tests. They provide the payload sizes the tests rely on, L2
//! block chains, the pair of configs a test builds its encoder from, the derivation-side reader
//! that turns the encoder's submissions back into batches, and the encoder states several tests
//! start from.

use std::{collections::HashSet, sync::Arc};

use alloy_consensus::{BlockBody, Header, SignableTransaction, TxLegacy};
use alloy_primitives::{B256, Bytes, Sealed, Signature};
use base_batcher_encoder::{
    BatchComposer, BatchEncoder, BatchPipeline, BatchSubmission, BlobPayload, DaType,
    EncoderConfig, FrameEncoder, StepResult, SubmissionPayload,
};
use base_blobs::BlobEncoder;
use base_common_consensus::{BaseBlock, BaseTxEnvelope, TxDeposit};
use base_common_genesis::{RollupConfig, UpgradeConfig};
use base_consensus_derive::{
    BlobData, ChannelAssembler, ChannelReaderProvider, PipelineError, PipelineErrorKind,
    test_utils::TestNextFrameProvider,
};
use base_protocol::{Batch, BatchReader, Frame, L1BlockInfoBedrock, L1BlockInfoTx, SingleBatch};
use futures::executor::block_on;
use rand::{RngCore, SeedableRng, rngs::SmallRng};

/// Time of every fixture block and of the Holocene activation. The L1 origin derivation reads at
/// is the default block, also at time 0, so the same forks are active on L2 for the encoder and
/// on L1 for derivation.
pub const GENESIS_TIMESTAMP: u64 = 0;

/// The derivation channel timeout, in L1 blocks, of the tests that exercise it.
pub const CHANNEL_TIMEOUT: u64 = 2;

/// A frame size, in bytes, small enough that a block of [`MULTI_FRAME_PAYLOAD`] spans several
/// frames.
pub const SMALL_FRAME_SIZE: usize = 32;

/// A block payload, in bytes, that spans several frames of [`SMALL_FRAME_SIZE`].
pub const MULTI_FRAME_PAYLOAD: usize = 200;

/// A block payload, in bytes, larger than one blob.
pub const MULTI_BLOB_PAYLOAD: usize = 200_000;

/// A block payload, in bytes, that fills two blobs and part of a third.
pub const THREE_BLOB_PAYLOAD: usize = 300_000;

/// Reads what an encoder hands out.
#[derive(Debug)]
pub struct SubmissionFixture;

impl SubmissionFixture {
    /// Every submission `encoder` has ready, in order.
    pub fn drain(encoder: &mut BatchEncoder) -> Vec<BatchSubmission> {
        std::iter::from_fn(|| encoder.next_submission()).collect()
    }

    /// The frames `submission` carries, in order.
    pub fn frames(submission: &BatchSubmission) -> Vec<Arc<Frame>> {
        match submission.payload() {
            SubmissionPayload::Blobs(blobs) => {
                blobs.iter().flat_map(BlobPayload::frames).cloned().collect()
            }
            SubmissionPayload::Calldata(frame) => vec![Arc::clone(frame)],
        }
    }
}

/// Two channels sharing a blob, on an encoder that closes each channel on its first block.
/// The first channel fills a blob on its own, then its tail and the second channel share the
/// next one.
#[derive(Debug)]
pub struct SharedBlob {
    /// The two blocks, one per channel.
    pub blocks: Vec<BaseBlock>,
    /// The blob the first channel fills.
    pub first: BatchSubmission,
    /// The transaction whose first blob carries the first channel's tail and the start of the
    /// second channel.
    pub packed: BatchSubmission,
}

impl SharedBlob {
    /// Feed `encoder` the two blocks and take the two submissions, checking that the first tail
    /// is held back until the second channel fills the blob, and that the blob carries frames of
    /// both channels.
    pub fn encode(encoder: &mut BatchEncoder) -> Self {
        let blocks = BlockFixture::chain(2, MULTI_BLOB_PAYLOAD);
        for block in &blocks {
            encoder.add_block(block.clone()).unwrap();
        }
        assert_eq!(encoder.step().unwrap(), StepResult::ChannelClosed);
        let first = encoder.next_submission().expect("the first channel fills a blob");
        assert!(encoder.next_submission().is_none(), "its tail waits for more data");
        assert_eq!(encoder.step().unwrap(), StepResult::ChannelClosed);
        let packed = encoder.next_submission().expect("the tail and the next channel fill a blob");
        let SubmissionPayload::Blobs(blobs) = packed.payload() else {
            panic!("expected a blob submission");
        };
        let channels: HashSet<_> = blobs[0].frames().iter().map(|frame| frame.id).collect();
        assert_eq!(channels.len(), 2, "the blob carries frames of both channels");
        Self { blocks, first, packed }
    }
}

/// Two channels retried in one transaction. The first channel's only blob and the second
/// channel's tail are requeued together once the second channel's two full blobs went out.
#[derive(Debug)]
pub struct SharedRetry {
    /// The two blocks, one per channel.
    pub blocks: Vec<BaseBlock>,
    /// The two full blobs of the second channel.
    pub full_blobs: BatchSubmission,
    /// The transaction carrying the first channel's blob and the second channel's tail.
    pub retry: BatchSubmission,
}

impl SharedRetry {
    /// Feed the two blocks to an encoder built with
    /// [`one_channel_per_block(2)`](EncoderFixture::one_channel_per_block), and requeue the two
    /// submissions that come back in one transaction.
    pub fn encode(encoder: &mut BatchEncoder) -> Self {
        let first_block = BlockFixture::block(B256::ZERO, 1, 0);
        let second_block =
            BlockFixture::block(first_block.header.hash_slow(), 2, THREE_BLOB_PAYLOAD);
        encoder.add_block(first_block.clone()).unwrap();
        let [first] = <[_; 1]>::try_from(encoder.encode_and_drain().unwrap()).unwrap();
        encoder.add_block(second_block.clone()).unwrap();
        let mut second = encoder.encode_and_drain().unwrap();
        assert_eq!(second.len(), 2, "two full blobs, then the tail");
        let tail = second.pop().expect("the tail");
        let full_blobs = second.pop().expect("the full blobs");

        encoder.requeue(first.id);
        encoder.requeue(tail.id);
        let retry = encoder.next_submission().expect("the retry");
        assert_eq!(retry.blob_count(), 2, "one transaction carries both channels");
        Self { blocks: vec![first_block, second_block], full_blobs, retry }
    }
}

/// An open channel that has handed out its first submission, fed blocks of
/// [`MULTI_BLOB_PAYLOAD`] one at a time until the encoder released output.
#[derive(Debug)]
pub struct OpenChannel {
    /// The chain the blocks come from.
    pub blocks: Vec<BaseBlock>,
    /// How many of them the encoder took.
    pub added: usize,
    /// The first submission the channel released.
    pub first: BatchSubmission,
}

impl OpenChannel {
    /// More blocks than any configuration needs before the channel releases output.
    const MAX_BLOCKS: u64 = 32;

    /// Feed `encoder` blocks until it releases a submission. Brotli may hold back output
    /// until it has seen enough input, so the number of blocks it takes is not fixed.
    pub fn encode(encoder: &mut BatchEncoder) -> Self {
        let blocks = BlockFixture::chain(Self::MAX_BLOCKS, MULTI_BLOB_PAYLOAD);
        let mut added = 0;
        let first = loop {
            let block = blocks.get(added).expect("the channel releases output within MAX_BLOCKS");
            encoder.add_block(block.clone()).unwrap();
            assert_eq!(encoder.step().unwrap(), StepResult::BlockEncoded);
            added += 1;
            if let Some(first) = encoder.next_submission() {
                break first;
            }
        };
        Self { blocks, added, first }
    }

    /// Feed `encoder` the next block, which the open channel takes.
    pub fn add_next_block(&mut self, encoder: &mut BatchEncoder) {
        encoder.add_block(self.blocks[self.added].clone()).unwrap();
        assert_eq!(encoder.step().unwrap(), StepResult::BlockEncoded);
        self.added += 1;
    }

    /// The batches of every block the encoder took.
    pub fn batches(&self) -> Vec<SingleBatch> {
        BlockFixture::batches(&self.blocks[..self.added])
    }
}

/// L2 block fixtures the encoder accepts.
#[derive(Debug)]
pub struct BlockFixture;

impl BlockFixture {
    /// A chain of `len` blocks numbered from 1, the first one built on parent hash zero.
    ///
    /// Each block carries the L1-info deposit and one user transaction whose input is
    /// `payload_len` pseudo-random bytes, so its channel data does not compress away.
    pub fn chain(len: u64, payload_len: usize) -> Vec<BaseBlock> {
        let mut parent_hash = B256::ZERO;
        (1..=len)
            .map(|number| {
                let block = Self::block(parent_hash, number, payload_len);
                parent_hash = block.header.hash_slow();
                block
            })
            .collect()
    }

    /// The batches derivation must read back from `blocks`. They come from the encoder's own
    /// composer because the round trip checks encoding, not composition, which has its own tests.
    pub fn batches(blocks: &[BaseBlock]) -> Vec<SingleBatch> {
        blocks
            .iter()
            .map(|block| {
                BatchComposer::block_to_single_batch(block).expect("fixture blocks should compose")
            })
            .collect()
    }

    /// A block numbered `number` on `parent_hash`, built like the blocks of
    /// [`chain`](Self::chain).
    pub fn block(parent_hash: B256, number: u64, payload_len: usize) -> BaseBlock {
        let calldata = L1BlockInfoTx::Bedrock(L1BlockInfoBedrock::default()).encode_calldata();
        let deposit = TxDeposit { input: calldata, ..Default::default() };
        let user_tx = TxLegacy { input: Self::noise(number, payload_len), ..Default::default() }
            .into_signed(Signature::test_signature());
        BaseBlock {
            header: Header {
                parent_hash,
                number,
                timestamp: GENESIS_TIMESTAMP,
                ..Default::default()
            },
            body: BlockBody {
                transactions: vec![
                    BaseTxEnvelope::Deposit(Sealed::new(deposit)),
                    BaseTxEnvelope::Legacy(user_tx),
                ],
                ..Default::default()
            },
        }
    }

    /// `len` pseudo-random bytes determined by `seed`.
    fn noise(seed: u64, len: usize) -> Bytes {
        let mut bytes = vec![0; len];
        SmallRng::seed_from_u64(seed).fill_bytes(&mut bytes);
        bytes.into()
    }
}

/// The pair of configs a test builds its encoder from, kept together so the encoder's output
/// is read back under the same pair.
#[derive(Debug)]
pub struct EncoderFixture {
    /// The rollup config, shared with the encoder.
    rollup_config: Arc<RollupConfig>,
    /// The encoder config.
    config: EncoderConfig,
}

impl EncoderFixture {
    /// A fixture for `config` on the default rollup config, with Holocene active from genesis
    /// because `derive` runs its channel stage. Holocene implies Granite and Fjord. Panics if
    /// production would reject the pair.
    pub fn new(config: EncoderConfig) -> Self {
        Self::with_rollup_config(config, RollupConfig::default())
    }

    /// Like [`new`](Self::new), with both channel timeouts set to [`CHANNEL_TIMEOUT`].
    fn with_channel_timeout(config: EncoderConfig) -> Self {
        let rollup_config = RollupConfig {
            channel_timeout: CHANNEL_TIMEOUT,
            granite_channel_timeout: CHANNEL_TIMEOUT,
            ..RollupConfig::default()
        };
        Self::with_rollup_config(config, rollup_config)
    }

    /// Calldata frames of [`SMALL_FRAME_SIZE`], so a block of [`MULTI_FRAME_PAYLOAD`] spans
    /// several transactions, and a channel duration of one L1 block under [`CHANNEL_TIMEOUT`].
    pub fn small_calldata_frames() -> Self {
        let config = EncoderConfig {
            da_type: DaType::Calldata,
            max_frame_size: SMALL_FRAME_SIZE,
            max_channel_duration: 1,
            ..EncoderConfig::default()
        };
        Self::with_channel_timeout(config)
    }

    /// Blobs with one channel per block, `max_blobs_per_tx` blobs per transaction, and a
    /// channel duration of one L1 block under [`CHANNEL_TIMEOUT`].
    pub fn one_channel_per_block(max_blobs_per_tx: usize) -> Self {
        let config = EncoderConfig {
            compressed_size_target: Some(1),
            max_blobs_per_tx,
            max_channel_duration: 1,
            ..EncoderConfig::default()
        };
        Self::with_channel_timeout(config)
    }

    fn with_rollup_config(config: EncoderConfig, rollup_config: RollupConfig) -> Self {
        let rollup_config = RollupConfig {
            upgrades: UpgradeConfig {
                holocene_time: Some(GENESIS_TIMESTAMP),
                ..UpgradeConfig::default()
            },
            ..rollup_config
        };
        config
            .validate_for_rollup_config(&rollup_config, GENESIS_TIMESTAMP)
            .expect("the test config should be one production accepts");
        Self { rollup_config: Arc::new(rollup_config), config }
    }

    /// A fresh encoder.
    pub fn encoder(&self) -> BatchEncoder {
        BatchEncoder::new(Arc::clone(&self.rollup_config), self.config.clone())
            .expect("the config was validated")
    }

    /// The batches derivation reads from `submissions`, one list per channel, in the order the
    /// channels complete. The submissions are taken as L1 transactions included in this order,
    /// all at the genesis L1 origin, so no channel times out.
    ///
    /// The frames go through the derivation `ChannelAssembler` and the channel data through the
    /// `BatchReader` derivation uses. Panics when a payload does not parse, when derivation drops
    /// a channel or leaves one without its terminal frame, and when channel data does not
    /// decompress or decode. Also panics on output derivation would accept but the encoder never
    /// emits, such as a channel id used twice, a channel not compressed with Brotli, a span batch,
    /// a frame above `max_frame_size`, a blob payload that does not fit a blob, or a transaction
    /// with zero or more than `max_blobs_per_tx` blobs. Batch validity (epoch, timestamp,
    /// sequence window) is out of scope.
    pub fn derive(&self, submissions: &[BatchSubmission]) -> Vec<Vec<SingleBatch>> {
        let frames: Vec<Frame> =
            submissions.iter().flat_map(|submission| self.wire_frames(submission)).collect();
        let mut channel_ids = HashSet::new();
        let mut first_frames = HashSet::new();
        for frame in &frames {
            assert!(
                frame.encoded_len() <= self.config.max_frame_size,
                "frame {} of channel {:?} is {} bytes, above max_frame_size {}",
                frame.number,
                frame.id,
                frame.encoded_len(),
                self.config.max_frame_size
            );
            channel_ids.insert(frame.id);
            if frame.number == 0 {
                assert!(first_frames.insert(frame.id), "channel id {:?} used twice", frame.id);
            }
        }

        // The test provider hands out its frames from the back of the list.
        let provider = TestNextFrameProvider::new(frames.into_iter().rev().map(Ok).collect());
        let mut assembler = ChannelAssembler::new(Arc::clone(&self.rollup_config), provider);
        let max_rlp_bytes = self.rollup_config.max_rlp_bytes_per_channel(GENESIS_TIMESTAMP);
        let mut channels = Vec::new();
        loop {
            match block_on(assembler.next_data()) {
                Ok(Some(data)) => channels.push(self.read_batches(&data, max_rlp_bytes)),
                Err(PipelineErrorKind::Temporary(PipelineError::Eof)) => break,
                Ok(None) | Err(PipelineErrorKind::Temporary(PipelineError::NotEnoughData)) => {}
                Err(error) => panic!("derivation failed on the frames: {error}"),
            }
        }
        assert!(assembler.channel.is_none(), "a channel has no terminal frame");
        assert_eq!(channels.len(), channel_ids.len(), "derivation dropped a channel");
        channels
    }

    /// The frames derivation parses from the L1 transaction built from `submission`, in
    /// order.
    fn wire_frames(&self, submission: &BatchSubmission) -> Vec<Frame> {
        match submission.payload() {
            SubmissionPayload::Calldata(frame) => {
                let calldata = FrameEncoder::to_calldata(frame);
                Frame::parse_frames(&calldata).expect("calldata should parse")
            }
            SubmissionPayload::Blobs(blobs) => {
                assert!(
                    (1..=self.config.max_blobs_per_tx).contains(&blobs.len()),
                    "{} blobs in one transaction, outside 1..={}",
                    blobs.len(),
                    self.config.max_blobs_per_tx
                );
                blobs
                    .iter()
                    .flat_map(|blob| {
                        let encoded = BlobEncoder::encode_packed(blob.frames())
                            .expect("the blob payload should fit a blob");
                        let data = BlobData {
                            data: Some(Bytes::copy_from_slice(encoded.as_slice())),
                            calldata: None,
                        }
                        .decode()
                        .expect("the blob should decode");
                        Frame::parse_frames(&data).expect("blob data should parse")
                    })
                    .collect()
            }
        }
    }

    /// Decode channel `data` with the reader the derivation channel stage uses.
    fn read_batches(&self, data: &[u8], max_rlp_bytes: u64) -> Vec<SingleBatch> {
        assert_eq!(
            data.first(),
            Some(&BatchReader::CHANNEL_VERSION_BROTLI),
            "channel data does not start with the Brotli version byte"
        );
        let brotli_supported = self.rollup_config.is_fjord_active(GENESIS_TIMESTAMP);
        let mut reader = BatchReader::new(data, max_rlp_bytes as usize, brotli_supported);

        let mut batches = Vec::new();
        while let Some(batch) = reader
            .next_batch_strict(&self.rollup_config)
            .expect("channel data should decompress and decode")
        {
            let Batch::Single(batch) = batch else {
                panic!("the encoder emits single batches, got a span batch");
            };
            batches.push(batch);
        }
        batches
    }
}
