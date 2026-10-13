use std::{collections::HashMap, sync::Arc};

use alloy_consensus::BlockBody;
use alloy_eips::eip4844::Blob;
use alloy_primitives::{Bytes, Sealed};
use base_batcher_encoder::{
    BatchEncoder, BatchPipeline, EncoderConfig, FrameEncoder, SubmissionPayload,
};
use base_blobs::{BlobDecoder, BlobEncoder};
use base_common_chains::ChainConfig;
use base_common_consensus::{
    BaseBlock, BaseTxEnvelope, L1BlockInfoBedrock, L1BlockInfoTx, OpTxType, TxDeposit,
    decode_2718_canonical,
};
use base_common_genesis::RollupConfig;
use base_protocol::{Batch, BatchReader, BlockInfo, Channel, ChannelId, Frame, SingleBatch};
use eyre::{Result, ensure, eyre};

use crate::Fixture;

/// L2 blocks and expected derivation output prepared once from captured fixtures.
///
/// Each fixture becomes one L2 block: its real header plus only the selected transactions.
/// Partial selections are batching/derivation component inputs, **not** the canonical batch the
/// mainnet batcher posted. When the L1-info deposit (index 0) was not captured, a Bedrock-format
/// L1-info deposit with a zero epoch is synthesized, because [`BatchEncoder`] requires one to
/// compose a batch; see [`Self::synthetic_l1_info`].
#[derive(Debug, Clone)]
pub struct BatchWorkload {
    /// Base mainnet rollup rules shared by encoding and decoding.
    pub rollup_config: Arc<RollupConfig>,
    /// Encoder settings, validated against `rollup_config` at the first block's timestamp.
    pub config: EncoderConfig,
    /// Chained blocks fed to [`BatchEncoder`] in order.
    pub blocks: Vec<BaseBlock>,
    /// Batches derivation must read back, built from fixture bytes rather than the encoder's
    /// composer: header parent hash/timestamp, L1-info epoch, and non-deposit transactions.
    pub expected: Vec<SingleBatch>,
    /// Blocks whose L1-info deposit was synthesized because it was not selected.
    pub synthetic_l1_info: usize,
    /// Non-deposit transactions carried by the batches.
    pub user_transactions: usize,
    /// Canonical EIP-2718 bytes of those transactions, before batch RLP and compression.
    pub user_bytes: usize,
}

/// One L1 transaction's derivation payload, in the form L1 carries it.
#[derive(Debug, Clone)]
pub enum DaPayload {
    /// `DERIVATION_VERSION_0 ++ frame`, as batcher calldata.
    Calldata(Bytes),
    /// EIP-4844 blobs of one transaction, each carrying packed frames.
    Blobs(Vec<Box<Blob>>),
}

/// Production encoder output for a whole [`BatchWorkload`].
#[derive(Debug, Clone)]
pub struct EncodedBatches {
    /// L1 transaction payloads in submission order.
    pub payloads: Vec<DaPayload>,
    /// Derivation frames across all payloads.
    pub frames: usize,
    /// Compressed channel bytes carried by those frames, excluding frame headers.
    pub channel_bytes: usize,
}

impl BatchWorkload {
    /// Validates fixtures and prepares blocks. Fixtures must be consecutive Base mainnet blocks
    /// (each header's parent is the previous fixture's block); pass one fixture per block.
    pub fn new(fixtures: &[Fixture], config: EncoderConfig) -> Result<Self> {
        let first = fixtures.first().ok_or_else(|| eyre!("no fixtures"))?;
        let rollup_config = Arc::new(ChainConfig::mainnet().rollup_config());
        config.validate_for_rollup_config(&rollup_config, first.header.timestamp)?;
        ensure!(
            fixtures.windows(2).all(|pair| pair[1].header.parent_hash == pair[0].block_hash),
            "fixtures must be consecutive blocks in chain order"
        );

        let mut workload = Self {
            rollup_config,
            config,
            blocks: Vec::with_capacity(fixtures.len()),
            expected: Vec::with_capacity(fixtures.len()),
            synthetic_l1_info: 0,
            user_transactions: 0,
            user_bytes: 0,
        };
        for fixture in fixtures {
            fixture.validate()?;
            let mut transactions = fixture
                .transactions
                .iter()
                .map(|tx| Ok(decode_2718_canonical::<BaseTxEnvelope>(&tx.encoded)?))
                .collect::<Result<Vec<_>>>()?;
            let captured_info = fixture.transactions[0].index == 0;
            if !captured_info {
                let input = L1BlockInfoTx::Bedrock(L1BlockInfoBedrock::default()).encode_calldata();
                let deposit = TxDeposit { input, ..Default::default() };
                transactions.insert(0, BaseTxEnvelope::Deposit(Sealed::new(deposit)));
                workload.synthetic_l1_info += 1;
            }
            let info = transactions[0]
                .as_deposit()
                .ok_or_else(|| eyre!("block {} index 0 is not a deposit", fixture.header.number))?;
            let epoch = L1BlockInfoTx::decode_calldata(&info.input)
                .map_err(|error| eyre!("invalid L1-info deposit: {error}"))?
                .id();
            let user: Vec<Bytes> = fixture
                .transactions
                .iter()
                .filter(|tx| tx.encoded.first() != Some(&(OpTxType::Deposit as u8)))
                .map(|tx| tx.encoded.clone())
                .collect();
            workload.user_transactions += user.len();
            workload.user_bytes += user.iter().map(|tx| tx.len()).sum::<usize>();
            workload.expected.push(SingleBatch {
                parent_hash: fixture.header.parent_hash,
                epoch_num: epoch.number,
                epoch_hash: epoch.hash,
                timestamp: fixture.header.timestamp,
                transactions: user,
            });
            workload.blocks.push(BaseBlock {
                header: fixture.header.clone(),
                body: BlockBody { transactions, ..Default::default() },
            });
        }
        Ok(workload)
    }

    /// Runs a fresh [`BatchEncoder`] over all blocks, flushes, and encodes every submission as
    /// L1 calldata ([`FrameEncoder`]) or blobs ([`BlobEncoder::encode_packed`]).
    ///
    /// Includes block cloning into the encoder (it takes ownership), batch composition, Brotli
    /// compression, framing, and DA packing. Channel IDs are random, so payload bytes differ per
    /// run while sizes and decoded batches do not.
    pub fn encode(&self) -> Result<EncodedBatches> {
        let mut encoder = BatchEncoder::new(Arc::clone(&self.rollup_config), self.config.clone())?;
        for block in &self.blocks {
            encoder.add_block(block.clone()).map_err(|(error, _)| error)?;
        }
        let mut encoded = EncodedBatches { payloads: Vec::new(), frames: 0, channel_bytes: 0 };
        for submission in encoder.encode_and_drain()? {
            encoded.frames += submission.frame_count();
            encoded.channel_bytes += submission.frame_bytes();
            encoded.payloads.push(match submission.payload() {
                SubmissionPayload::Calldata(frame) => {
                    DaPayload::Calldata(FrameEncoder::to_calldata(frame))
                }
                SubmissionPayload::Blobs(blobs) => DaPayload::Blobs(
                    blobs
                        .iter()
                        .map(|blob| BlobEncoder::encode_packed(blob.frames()))
                        .collect::<Result<_, _>>()?,
                ),
            });
        }
        Ok(encoded)
    }

    /// Reads batches back as derivation does: blob decoding, [`Frame::parse_frames`], channel
    /// reassembly, then [`BatchReader`] decompression and decoding, in channel completion order.
    ///
    /// All frames are treated as included at one L1 block, so channel timeouts do not apply.
    /// Like Holocene, each channel's frames must arrive in order. Fork limits use the first
    /// block's L2 timestamp in place of an L1 origin. Batch validity (epoch, sequencing window)
    /// is out of scope. Errors on malformed data, span batches, or unfinished channels.
    pub fn decode(&self, encoded: &EncodedBatches) -> Result<Vec<SingleBatch>> {
        let timestamp = self.blocks.first().map_or(0, |block| block.header.timestamp);
        let max_rlp_bytes = self.rollup_config.max_rlp_bytes_per_channel(timestamp) as usize;
        let brotli_supported = self.rollup_config.is_fjord_active(timestamp);
        let mut channels = HashMap::<ChannelId, Channel>::new();
        let mut batches = Vec::with_capacity(self.expected.len());
        let mut read = |frames: Vec<Frame>| -> Result<()> {
            for frame in frames {
                let channel = channels
                    .entry(frame.id)
                    .or_insert_with(|| Channel::new(frame.id, Default::default()));
                ensure!(usize::from(frame.number) == channel.len(), "out-of-order frame");
                channel.add_frame(frame, BlockInfo::default())?;
                if !channel.is_ready() {
                    continue;
                }
                let id = channel.id();
                let data = channel.frame_data().ok_or_else(|| eyre!("incomplete channel"))?;
                channels.remove(&id);
                let mut reader = BatchReader::new(&data[..], max_rlp_bytes, brotli_supported);
                while let Some(batch) = reader.next_batch_strict(&self.rollup_config)? {
                    let Batch::Single(batch) = batch else {
                        return Err(eyre!("unexpected span batch"));
                    };
                    batches.push(batch);
                }
            }
            Ok(())
        };
        for payload in &encoded.payloads {
            match payload {
                DaPayload::Calldata(data) => read(Frame::parse_frames(data)?)?,
                DaPayload::Blobs(blobs) => {
                    for blob in blobs {
                        read(Frame::parse_frames(&BlobDecoder::decode(blob)?)?)?;
                    }
                }
            }
        }
        ensure!(channels.is_empty(), "{} channels without a terminal frame", channels.len());
        Ok(batches)
    }

    /// Checks decoded batches against [`Self::expected`], identifying the first mismatch.
    pub fn verify(&self, decoded: &[SingleBatch]) -> Result<()> {
        ensure!(
            decoded.len() == self.expected.len(),
            "decoded {} batches, expected {}",
            decoded.len(),
            self.expected.len()
        );
        for (index, (decoded, expected)) in decoded.iter().zip(&self.expected).enumerate() {
            ensure!(decoded == expected, "batch {index} does not round trip");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use base_batcher_encoder::DaType;

    use super::*;

    fn fixture() -> Fixture {
        serde_json::from_str(include_str!("../fixtures/mainnet.json")).unwrap()
    }

    /// Mainnet user transactions survive production encoding and derivation-side decoding
    /// across blob and multi-frame calldata shapes, and corrupt data is rejected.
    #[test]
    fn mainnet_transactions_round_trip_through_encoder_and_reader() {
        let fixture = fixture();
        let small_calldata =
            EncoderConfig { da_type: DaType::Calldata, max_frame_size: 64, ..Default::default() };
        for (config, multi_frame) in [(EncoderConfig::default(), false), (small_calldata, true)] {
            let workload = BatchWorkload::new(std::slice::from_ref(&fixture), config).unwrap();
            assert_eq!(workload.synthetic_l1_info, 0);
            assert_eq!(workload.user_transactions, 2);
            assert_eq!(workload.expected[0].transactions[0], fixture.transactions[1].encoded);
            let encoded = workload.encode().unwrap();
            assert_eq!(encoded.frames > 1, multi_frame);
            assert!(encoded.channel_bytes > 0);
            workload.verify(&workload.decode(&encoded).unwrap()).unwrap();

            let mut truncated = encoded.clone();
            truncated.payloads.pop();
            let decoded = workload.decode(&truncated);
            assert!(decoded.and_then(|decoded| workload.verify(&decoded)).is_err());
        }

        let workload = BatchWorkload::new(&[fixture], EncoderConfig::default()).unwrap();
        let mut encoded = workload.encode().unwrap();
        let DaPayload::Blobs(blobs) = &mut encoded.payloads[0] else { panic!("expected blobs") };
        blobs[0][200] ^= 1;
        assert!(workload.decode(&encoded).and_then(|decoded| workload.verify(&decoded)).is_err());
    }

    /// Selections without the L1-info deposit still compose, and inputs that cannot form a
    /// chain are rejected before encoding.
    #[test]
    fn selections_without_l1_info_compose_and_non_chains_fail() {
        let mut fixture = fixture();
        fixture.transactions.remove(0);
        let workload =
            BatchWorkload::new(std::slice::from_ref(&fixture), EncoderConfig::default()).unwrap();
        assert_eq!(workload.synthetic_l1_info, 1);
        assert_eq!(workload.expected[0].epoch_num, 0);
        workload.verify(&workload.decode(&workload.encode().unwrap()).unwrap()).unwrap();

        assert!(BatchWorkload::new(&[fixture.clone(), fixture], EncoderConfig::default()).is_err());
        assert!(BatchWorkload::new(&[], EncoderConfig::default()).is_err());
    }
}
