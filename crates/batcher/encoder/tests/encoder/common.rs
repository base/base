//! Fixtures shared by the encoder tests. They provide L2 block chains, the pair of configs a
//! test builds its encoder from, and the derivation-side reader that turns the encoder's
//! submissions back into batches.

use std::{collections::HashSet, sync::Arc};

use alloy_consensus::{BlockBody, Header, SignableTransaction, TxLegacy};
use alloy_primitives::{B256, Bytes, Sealed, Signature};
use base_batcher_encoder::{
    BatchComposer, BatchEncoder, BatchSubmission, EncoderConfig, FrameEncoder, SubmissionPayload,
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

    fn block(parent_hash: B256, number: u64, payload_len: usize) -> BaseBlock {
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
        let rollup_config = RollupConfig {
            upgrades: UpgradeConfig {
                holocene_time: Some(GENESIS_TIMESTAMP),
                ..UpgradeConfig::default()
            },
            ..RollupConfig::default()
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
            submissions.iter().flat_map(|submission| self.frames(submission)).collect();
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
    fn frames(&self, submission: &BatchSubmission) -> Vec<Frame> {
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
