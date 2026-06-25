//! Submission normalization and comparison.

use std::collections::{HashMap, hash_map::Entry};

use alloy_eips::eip4844::Blob;
use alloy_primitives::keccak256;
use base_blobs::BlobDecoder;
use base_common_genesis::RollupConfig;
use base_protocol::{Batch, BatchReader, BlockInfo, Channel, ChannelId, Frame};

use crate::{
    NormalizedBatch, NormalizedBatchKind, NormalizedChannel, NormalizedSubmission,
    ParityComparison, ParityError,
};

/// Normalizes batcher DA submissions for stable parity comparison.
#[derive(Debug)]
pub struct ParityNormalizer;

impl ParityNormalizer {
    /// Normalize a calldata submission payload.
    ///
    /// `data` must be the L1 transaction input beginning with the derivation
    /// version byte.
    pub fn normalize_calldata(
        data: &[u8],
        inclusion_timestamp: u64,
        rollup_config: &RollupConfig,
    ) -> Result<NormalizedSubmission, ParityError> {
        let frames = Frame::parse_frames(data)?;
        Ok(Self::normalize_frames(frames, inclusion_timestamp, rollup_config))
    }

    /// Normalize a blob submission payload.
    pub fn normalize_blob(
        blob: &Blob,
        inclusion_timestamp: u64,
        rollup_config: &RollupConfig,
    ) -> Result<NormalizedSubmission, ParityError> {
        let data = BlobDecoder::decode(blob)?;
        Self::normalize_calldata(data.as_ref(), inclusion_timestamp, rollup_config)
    }

    /// Normalize already-decoded frames.
    pub fn normalize_frames(
        frames: impl IntoIterator<Item = Frame>,
        inclusion_timestamp: u64,
        rollup_config: &RollupConfig,
    ) -> NormalizedSubmission {
        let block_info = BlockInfo { timestamp: inclusion_timestamp, ..Default::default() };
        let mut channels: HashMap<ChannelId, Channel> = HashMap::new();
        let mut channel_order = Vec::new();
        let mut rejected_frames = 0usize;

        for frame in frames {
            let frame_id = frame.id;
            let channel = match channels.entry(frame_id) {
                Entry::Occupied(entry) => entry.into_mut(),
                Entry::Vacant(entry) => {
                    channel_order.push(frame_id);
                    entry.insert(Channel::new(frame_id, block_info))
                }
            };
            if channel.add_frame(frame, block_info).is_err() {
                rejected_frames += 1;
            }
        }

        let mut complete_channels = 0usize;
        let mut ready_channels = 0usize;
        let mut decode_errors = 0usize;
        let mut batches = Vec::new();
        for id in channel_order {
            let Some(channel) = channels.get(&id) else { continue };
            if !channel.is_ready() {
                continue;
            }
            ready_channels += 1;
            match Self::try_normalize_channel(channel, inclusion_timestamp, rollup_config) {
                Ok(decoded) => {
                    complete_channels += 1;
                    batches.extend(decoded.batches);
                }
                Err(_) => {
                    decode_errors += 1;
                }
            }
        }

        NormalizedSubmission {
            batches,
            complete_channels,
            incomplete_channels: channels.len().saturating_sub(ready_channels),
            rejected_frames,
            decode_errors,
        }
    }

    /// Normalize all batches from a complete channel.
    pub fn normalize_channel(
        channel: &Channel,
        inclusion_timestamp: u64,
        rollup_config: &RollupConfig,
    ) -> Vec<NormalizedBatch> {
        Self::try_normalize_channel(channel, inclusion_timestamp, rollup_config)
            .map(|channel| channel.batches)
            .unwrap_or_default()
    }

    /// Normalize all batches from a complete channel, preserving decode failures.
    pub fn try_normalize_channel(
        channel: &Channel,
        inclusion_timestamp: u64,
        rollup_config: &RollupConfig,
    ) -> Result<NormalizedChannel, ParityError> {
        let Some(data) = channel.frame_data() else {
            return Ok(NormalizedChannel { batches: Vec::new() });
        };
        let max_rlp = usize::try_from(rollup_config.max_rlp_bytes_per_channel(inclusion_timestamp))
            .expect("max RLP bytes per channel must fit in usize");
        let brotli_supported = rollup_config.is_fjord_active(inclusion_timestamp);
        let mut reader = BatchReader::new(data.to_vec(), max_rlp, brotli_supported);
        let mut batches = Vec::new();
        while let Some(batch) = reader.next_batch_strict(rollup_config)? {
            batches.push(Self::normalize_batch(&batch));
        }

        Ok(NormalizedChannel { batches })
    }

    /// Normalize a decoded batch.
    pub fn normalize_batch(batch: &Batch) -> NormalizedBatch {
        match batch {
            Batch::Single(batch) => NormalizedBatch {
                kind: NormalizedBatchKind::Single,
                parent_hash: Some(batch.parent_hash),
                epoch_hash: Some(batch.epoch_hash),
                parent_check: None,
                l1_origin_check: None,
                chain_id: None,
                origin_bits: None,
                start_timestamp: batch.timestamp,
                end_timestamp: batch.timestamp,
                start_epoch_num: batch.epoch_num,
                end_epoch_num: batch.epoch_num,
                block_count: 1,
                tx_counts: vec![
                    u64::try_from(batch.transactions.len())
                        .expect("transaction count must fit in u64"),
                ],
                tx_hashes: batch.transactions.iter().map(|tx| keccak256(tx.as_ref())).collect(),
            },
            Batch::Span(batch) => {
                let start = batch.batches.first();
                let end = batch.batches.last();
                NormalizedBatch {
                    kind: NormalizedBatchKind::Span,
                    parent_hash: None,
                    epoch_hash: None,
                    parent_check: Some(batch.parent_check),
                    l1_origin_check: Some(batch.l1_origin_check),
                    chain_id: Some(batch.chain_id),
                    origin_bits: Some(batch.origin_bits.as_ref().to_vec()),
                    start_timestamp: start.map_or(0, |batch| batch.timestamp),
                    end_timestamp: end.map_or(0, |batch| batch.timestamp),
                    start_epoch_num: start.map_or(0, |batch| batch.epoch_num),
                    end_epoch_num: end.map_or(0, |batch| batch.epoch_num),
                    block_count: u64::try_from(batch.batches.len())
                        .expect("span batch count must fit in u64"),
                    tx_counts: batch
                        .batches
                        .iter()
                        .map(|batch| {
                            u64::try_from(batch.transactions.len())
                                .expect("transaction count must fit in u64")
                        })
                        .collect(),
                    tx_hashes: batch
                        .batches
                        .iter()
                        .flat_map(|batch| batch.transactions.iter())
                        .map(|tx| keccak256(tx.as_ref()))
                        .collect(),
                }
            }
        }
    }
}

/// Compares normalized batcher submissions.
#[derive(Debug)]
pub struct ParityComparator;

impl ParityComparator {
    /// Compare two normalized batch lists.
    pub fn compare(left: &[NormalizedBatch], right: &[NormalizedBatch]) -> ParityComparison {
        let first_pair_mismatch = left.iter().zip(right).position(|(left, right)| left != right);
        let first_mismatch = first_pair_mismatch.or_else(|| {
            if left.len() == right.len() { None } else { Some(left.len().min(right.len())) }
        });

        ParityComparison {
            is_match: first_mismatch.is_none(),
            left_len: left.len(),
            right_len: right.len(),
            first_mismatch,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use alloy_eips::eip1898::BlockNumHash;
    use alloy_primitives::{B256, Bytes};
    use alloy_rlp::Encodable;
    use base_blobs::BlobEncoder;
    use base_common_genesis::{ChainGenesis, RollupConfig};
    use base_protocol::{Batch, BlockInfo, Channel, Frame, SingleBatch};

    use super::*;

    fn test_rollup_config() -> RollupConfig {
        RollupConfig {
            genesis: ChainGenesis {
                l2: BlockNumHash { number: 100, hash: B256::ZERO },
                ..Default::default()
            },
            block_time: 2,
            ..Default::default()
        }
    }

    fn encode_single_batch(batch: &SingleBatch) -> Vec<u8> {
        let typed_batch = Batch::Single(batch.clone());
        let mut batch_bytes = Vec::new();
        typed_batch.encode(&mut batch_bytes).expect("batch must encode");

        let mut rlp_buf = Vec::new();
        batch_bytes.as_slice().encode(&mut rlp_buf);
        miniz_oxide::deflate::compress_to_vec_zlib(&rlp_buf, 6)
    }

    fn single_frame(id: [u8; 16], data: Vec<u8>) -> Frame {
        Frame { id, number: 0, data, is_last: true }
    }

    fn calldata_from_frame(frame: &Frame) -> Vec<u8> {
        let mut data = vec![base_protocol::DERIVATION_VERSION_0];
        data.extend_from_slice(&frame.encode());
        data
    }

    #[test]
    fn normalization_ignores_channel_ids() {
        let rollup_config = test_rollup_config();
        let batch = SingleBatch {
            epoch_num: 123,
            timestamp: 1000,
            transactions: vec![Bytes::from_static(b"tx-a")],
            ..Default::default()
        };

        let left = calldata_from_frame(&single_frame(
            [1u8; Channel::ID_LENGTH],
            encode_single_batch(&batch),
        ));
        let right = calldata_from_frame(&single_frame(
            [2u8; Channel::ID_LENGTH],
            encode_single_batch(&batch),
        ));

        let left = ParityNormalizer::normalize_calldata(&left, 0, &rollup_config)
            .expect("left should normalize");
        let right = ParityNormalizer::normalize_calldata(&right, 0, &rollup_config)
            .expect("right should normalize");

        assert_eq!(left.batches, right.batches);
        assert!(ParityComparator::compare(&left.batches, &right.batches).is_match);
    }

    #[test]
    fn comparison_reports_transaction_mismatch() {
        let rollup_config = test_rollup_config();
        let left_batch = SingleBatch {
            epoch_num: 123,
            timestamp: 1000,
            transactions: vec![Bytes::from_static(b"tx-a")],
            ..Default::default()
        };
        let right_batch = SingleBatch {
            epoch_num: 123,
            timestamp: 1000,
            transactions: vec![Bytes::from_static(b"tx-b")],
            ..Default::default()
        };

        let left = calldata_from_frame(&single_frame(
            [1u8; Channel::ID_LENGTH],
            encode_single_batch(&left_batch),
        ));
        let right = calldata_from_frame(&single_frame(
            [1u8; Channel::ID_LENGTH],
            encode_single_batch(&right_batch),
        ));

        let left = ParityNormalizer::normalize_calldata(&left, 0, &rollup_config)
            .expect("left should normalize");
        let right = ParityNormalizer::normalize_calldata(&right, 0, &rollup_config)
            .expect("right should normalize");
        let comparison = ParityComparator::compare(&left.batches, &right.batches);

        assert!(!comparison.is_match);
        assert_eq!(comparison.first_mismatch, Some(0));
    }

    #[test]
    fn comparison_reports_parent_hash_mismatch() {
        let rollup_config = test_rollup_config();
        let left_batch = SingleBatch {
            parent_hash: B256::repeat_byte(0x11),
            epoch_num: 123,
            timestamp: 1000,
            transactions: vec![Bytes::from_static(b"tx-a")],
            ..Default::default()
        };
        let right_batch = SingleBatch {
            parent_hash: B256::repeat_byte(0x22),
            epoch_num: 123,
            timestamp: 1000,
            transactions: vec![Bytes::from_static(b"tx-a")],
            ..Default::default()
        };

        let left = calldata_from_frame(&single_frame(
            [1u8; Channel::ID_LENGTH],
            encode_single_batch(&left_batch),
        ));
        let right = calldata_from_frame(&single_frame(
            [1u8; Channel::ID_LENGTH],
            encode_single_batch(&right_batch),
        ));

        let left = ParityNormalizer::normalize_calldata(&left, 0, &rollup_config)
            .expect("left should normalize");
        let right = ParityNormalizer::normalize_calldata(&right, 0, &rollup_config)
            .expect("right should normalize");
        let comparison = ParityComparator::compare(&left.batches, &right.batches);

        assert!(!comparison.is_match);
        assert_eq!(comparison.first_mismatch, Some(0));
    }

    #[test]
    fn normalize_frames_preserves_first_seen_channel_order() {
        let rollup_config = test_rollup_config();
        let first_batch = SingleBatch { epoch_num: 123, timestamp: 1000, ..Default::default() };
        let second_batch = SingleBatch { epoch_num: 124, timestamp: 1002, ..Default::default() };
        let frames = vec![
            single_frame([2u8; Channel::ID_LENGTH], encode_single_batch(&first_batch)),
            single_frame([1u8; Channel::ID_LENGTH], encode_single_batch(&second_batch)),
        ];

        let normalized = ParityNormalizer::normalize_frames(frames, 0, &rollup_config);

        assert_eq!(normalized.batches.len(), 2);
        assert_eq!(normalized.batches[0].start_timestamp, 1000);
        assert_eq!(normalized.batches[1].start_timestamp, 1002);
    }

    #[test]
    fn normalizes_blob_submission() {
        let rollup_config = test_rollup_config();
        let batch = SingleBatch { epoch_num: 123, timestamp: 1000, ..Default::default() };
        let frame = Arc::new(single_frame([1u8; Channel::ID_LENGTH], encode_single_batch(&batch)));
        let blob = BlobEncoder::encode_packed(&[frame]).expect("blob should encode");

        let normalized = ParityNormalizer::normalize_blob(&blob, 0, &rollup_config)
            .expect("blob should normalize");

        assert_eq!(normalized.batches.len(), 1);
        assert_eq!(normalized.complete_channels, 1);
        assert_eq!(normalized.incomplete_channels, 0);
    }

    #[test]
    fn try_normalize_channel_reports_corrupted_channel_data() {
        let rollup_config = test_rollup_config();
        let block_info = BlockInfo::default();
        let id = [1u8; Channel::ID_LENGTH];
        let mut channel = Channel::new(id, block_info);
        channel
            .add_frame(single_frame(id, vec![0x02]), block_info)
            .expect("frame should be accepted");

        let err = ParityNormalizer::try_normalize_channel(&channel, 0, &rollup_config)
            .expect_err("corrupted channel must fail strict normalization");

        assert!(matches!(err, ParityError::ChannelDecompress(_)));
    }

    #[test]
    fn normalize_frames_counts_decode_errors() {
        let rollup_config = test_rollup_config();
        let id = [1u8; Channel::ID_LENGTH];
        let frames = vec![single_frame(id, vec![0x02])];

        let normalized = ParityNormalizer::normalize_frames(frames, 0, &rollup_config);

        assert_eq!(normalized.batches.len(), 0);
        assert_eq!(normalized.complete_channels, 0);
        assert_eq!(normalized.incomplete_channels, 0);
        assert_eq!(normalized.decode_errors, 1);
    }
}
