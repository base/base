//! Shared fixtures: L2 block chains, the pair of configs a test builds its encoder from, and
//! the derivation-side reader that turns the encoder's submissions back into batches.

use std::{collections::HashSet, sync::Arc};

use alloy_consensus::{BlockBody, Header, SignableTransaction, TxLegacy};
use alloy_primitives::{B256, Bytes, Sealed, Signature};
use base_batcher_encoder::{
    BatchComposer, BatchEncoder, BatchSubmission, EncoderConfig, FrameEncoder, SubmissionPayload,
};
use base_blobs::BlobEncoder;
use base_common_consensus::{BaseBlock, BaseTxEnvelope, TxDeposit};
use base_common_genesis::{RollupConfig, UpgradeConfig};
use base_consensus_derive::BlobData;
use base_protocol::{
    Batch, BatchReader, BlockInfo, Channel, Frame, L1BlockInfoBedrock, L1BlockInfoTx, SingleBatch,
};
use rand::{RngCore, SeedableRng, rngs::SmallRng};

/// Every fixture block and the rollup config are at time 0, so the forks the encoder relies
/// on are active from genesis, on L2 for the encoder and on L1 for derivation.
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
    /// composer, which has its own tests: the round trip is about encoding, not composition.
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
    /// `config` over the default rollup config with Holocene, hence Granite and Fjord, active
    /// from genesis. Only pairs production accepts are allowed.
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

    /// What derivation reads from `submissions`, taken as the L1 transactions they become, all
    /// included in this order within the channel timeout, at an L1 origin at genesis: the
    /// batches of each channel, in the order their terminal frames landed.
    ///
    /// Panics where derivation would drop data, under the strict frame order of Holocene: a
    /// transaction payload that does not parse, a frame out of order or from another channel
    /// than the open one, a channel replaced or left without its terminal frame, a channel
    /// above the RLP limit, and channel data that does not decompress or decode. Also enforces
    /// what the encoder promises beyond that: channel ids used once, Brotli channels, every
    /// frame within `max_frame_size`, every blob payload fitting a blob, and one to
    /// `max_blobs_per_tx` blobs per transaction. Batch validity (epoch, timestamp, sequence
    /// window) is out of scope.
    pub fn derive(&self, submissions: &[BatchSubmission]) -> Vec<Vec<SingleBatch>> {
        let max_rlp_bytes = self.rollup_config.max_rlp_bytes_per_channel(GENESIS_TIMESTAMP);
        let mut channels = Vec::new();
        let mut seen_ids = HashSet::new();
        let mut open: Option<Channel> = None;

        for frame in submissions.iter().flat_map(|submission| self.frames(submission)) {
            assert!(
                frame.encoded_len() <= self.config.max_frame_size,
                "frame {} of channel {:?} is {} bytes, above max_frame_size {}",
                frame.number,
                frame.id,
                frame.encoded_len(),
                self.config.max_frame_size
            );

            if frame.number == 0 {
                if let Some(open) = &open {
                    panic!("channel {:?} replaced before its terminal frame", open.id);
                }
                assert!(seen_ids.insert(frame.id), "channel id {:?} used twice", frame.id);
                open = Some(Channel::new(frame.id, BlockInfo::default()));
            }
            let Some(channel) = open.as_mut() else {
                panic!(
                    "frame {} of channel {:?} arrives with no channel open",
                    frame.number, frame.id
                )
            };
            assert_eq!(
                frame.id, channel.id,
                "frame {} of channel {:?} interleaved in channel {:?}",
                frame.number, frame.id, channel.id
            );
            assert_eq!(
                usize::from(frame.number),
                channel.len(),
                "frame {} of channel {:?} out of order",
                frame.number,
                frame.id
            );
            channel
                .add_frame(frame, BlockInfo::default())
                .expect("an in-order frame should be accepted");
            assert!(
                channel.size() as u64 <= max_rlp_bytes,
                "channel {:?} is above the RLP limit {max_rlp_bytes}",
                channel.id
            );

            let Some(channel) = open.take_if(|channel| channel.is_ready()) else {
                continue;
            };
            let data = channel.frame_data().expect("a ready channel should have its data");
            channels.push(self.read_batches(&data, max_rlp_bytes));
        }

        if let Some(open) = open {
            panic!("channel {:?} has no terminal frame", open.id);
        }
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
