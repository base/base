//! Attribution of an L2 transaction to complete, decoded L1 batch channels.

use std::collections::HashMap;

use alloy_primitives::{B256, Bytes, keccak256};
use base_common_genesis::RollupConfig;
use base_protocol::{Batch, BatchReader, BlockInfo, Channel, ChannelId, Frame};
use eyre::{Result, ensure};
use serde::Serialize;
use serde_json::Value;

/// Successful inbox transaction carrying channel frames.
#[derive(Clone, Debug, Serialize)]
pub struct Submission {
    /// L1 transaction and envelope fields as observed from the execution client.
    pub transaction: Value,
    /// Successful receipt, including the exact containing block hash and number.
    pub receipt: Value,
    /// Authenticated L1 block containing the submission.
    pub block: BlockInfo,
}

/// A transaction recovered from decoded batch data at its actual L2 timestamp/origin.
#[derive(Clone, Debug, Serialize)]
pub struct BatchAttribution {
    /// Channel identifier shared by all contributing frames.
    pub channel_id: String,
    /// Every successful L1 transaction needed to reconstruct this channel.
    pub submissions: Vec<Submission>,
    /// Hash of the exact signed L2 transaction recovered from the batch.
    pub transaction_hash: B256,
    /// Timestamp of the batch element containing the transaction.
    pub l2_timestamp: u64,
    /// L1 origin encoded in that batch element.
    pub l1_origin_number: u64,
}

/// Expected transaction identity anchored to its receipt's L2 block.
#[derive(Debug)]
pub struct TransferTarget {
    /// Exact EIP-2718 signed transaction submitted to L2.
    pub raw_transaction: Bytes,
    /// Timestamp of the receipt block.
    pub timestamp: u64,
    /// L1 origin number read from the receipt block's L1-info deposit.
    pub l1_origin_number: u64,
}

/// In-progress channel and its contributing successful inbox transactions.
#[derive(Debug)]
pub struct SubmittedChannel {
    /// Production channel reassembler.
    pub channel: Channel,
    /// Transactions carrying this channel's frames.
    pub submissions: Vec<Submission>,
}

/// Reassembles production frames and decodes batches, rather than counting sender activity.
#[derive(Debug, Default)]
pub struct BatchObserver {
    /// Channels not yet completely decoded.
    pub channels: HashMap<ChannelId, SubmittedChannel>,
}

impl BatchObserver {
    /// Consumes a calldata payload or decoded blob payload from a successful inbox submission.
    pub fn ingest(
        &mut self,
        payload: Bytes,
        submission: Submission,
        target: &TransferTarget,
        rollup: &RollupConfig,
    ) -> Result<Option<BatchAttribution>> {
        let mut attribution = None;
        for frame in Frame::parse_frames(&payload)? {
            let id = frame.id;
            let pending = self.channels.entry(id).or_insert_with(|| SubmittedChannel {
                channel: Channel::new(id, submission.block),
                submissions: Vec::new(),
            });
            pending.channel.add_frame(frame, submission.block)?;
            if !pending
                .submissions
                .iter()
                .any(|existing| existing.transaction["hash"] == submission.transaction["hash"])
            {
                pending.submissions.push(submission.clone());
            }
            ensure!(
                pending.channel.size()
                    <= rollup.max_rlp_bytes_per_channel(submission.block.timestamp) as usize,
                "observed channel exceeds the configured data limit"
            );
            if !pending.channel.is_ready() {
                continue;
            }
            let data = pending
                .channel
                .frame_data()
                .ok_or_else(|| eyre::eyre!("complete channel has no frame data"))?;
            let mut reader = BatchReader::new(
                data,
                rollup.max_rlp_bytes_per_channel(submission.block.timestamp) as usize,
                rollup.is_fjord_active(submission.block.timestamp),
            );
            // Decode the entire channel strictly: finding a target must not hide a malformed tail.
            while let Some(batch) = reader.next_batch_strict(rollup)? {
                let matches = match batch {
                    Batch::Single(batch) => {
                        batch.timestamp == target.timestamp
                            && batch.epoch_num == target.l1_origin_number
                            && batch.transactions.contains(&target.raw_transaction)
                    }
                    Batch::Span(batch) => batch.batches.iter().any(|element| {
                        element.timestamp == target.timestamp
                            && element.epoch_num == target.l1_origin_number
                            && element.transactions.contains(&target.raw_transaction)
                    }),
                };
                if matches {
                    attribution = Some(BatchAttribution {
                        channel_id: hex::encode(id),
                        submissions: pending.submissions.clone(),
                        transaction_hash: keccak256(&target.raw_transaction),
                        l2_timestamp: target.timestamp,
                        l1_origin_number: target.l1_origin_number,
                    });
                }
            }
            self.channels.remove(&id);
        }
        Ok(attribution)
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{B256, Bytes, keccak256};
    use base_common_genesis::RollupConfig;
    use base_protocol::{Batch, BlockInfo, Frame, SingleBatch};
    use miniz_oxide::{deflate::compress_to_vec_zlib, inflate::decompress_to_vec_zlib};
    use serde_json::json;

    use super::{BatchObserver, Submission, TransferTarget};

    fn target() -> TransferTarget {
        TransferTarget {
            raw_transaction: Bytes::from_static(&[2, 0xaa, 0xbb]),
            timestamp: 100,
            l1_origin_number: 10,
        }
    }

    fn submission(number: u64) -> Submission {
        Submission {
            transaction: json!({"hash": B256::with_last_byte(number as u8)}),
            receipt: json!({"status": "0x1"}),
            block: BlockInfo::new(B256::with_last_byte(number as u8), number, B256::ZERO, number),
        }
    }

    fn payloads(target: &TransferTarget) -> [Bytes; 2] {
        let batch = Batch::Single(SingleBatch {
            timestamp: target.timestamp,
            epoch_num: target.l1_origin_number,
            transactions: vec![target.raw_transaction.clone()],
            ..Default::default()
        });
        let mut encoded = Vec::new();
        batch.encode(&mut encoded).unwrap();
        let channel = compress_to_vec_zlib(&alloy_rlp::encode(Bytes::from(encoded)), 6);
        let middle = channel.len() / 2;
        [
            Frame::new([1; 16], 0, channel[..middle].to_vec(), false),
            Frame::new([1; 16], 1, channel[middle..].to_vec(), true),
        ]
        .map(|frame| {
            let mut payload = vec![0];
            payload.extend(frame.encode());
            payload.into()
        })
    }

    #[test]
    fn attributes_only_a_complete_channel_to_the_exact_transaction_and_block() {
        let target = target();
        let [first, last] = payloads(&target);
        let mut observer = BatchObserver::default();
        let config = RollupConfig::default();
        assert!(observer.ingest(first, submission(11), &target, &config).unwrap().is_none());
        let evidence = observer.ingest(last, submission(12), &target, &config).unwrap().unwrap();
        assert_eq!(evidence.transaction_hash, keccak256(&target.raw_transaction));
        assert_eq!(evidence.submissions.len(), 2);
        assert_eq!(evidence.submissions[0].block.number, 11);
        assert_eq!(evidence.submissions[1].block.number, 12);
    }

    #[test]
    fn does_not_count_unrelated_transactions_or_wrong_block_context() {
        for changed in [
            TransferTarget { timestamp: 102, ..target() },
            TransferTarget { l1_origin_number: 9, ..target() },
            TransferTarget { raw_transaction: Bytes::from_static(&[2, 0xcc]), ..target() },
        ] {
            let mut observer = BatchObserver::default();
            for payload in payloads(&target()) {
                assert!(
                    observer
                        .ingest(payload, submission(11), &changed, &RollupConfig::default())
                        .unwrap()
                        .is_none()
                );
            }
        }
    }

    #[test]
    fn rejects_a_malformed_tail_even_after_finding_the_target_transaction() {
        let compressed: Vec<u8> = payloads(&target())
            .into_iter()
            .flat_map(|payload| Frame::parse_frames(&payload).unwrap()[0].data.clone())
            .collect();
        let mut decoded = decompress_to_vec_zlib(&compressed).unwrap();
        decoded.push(0xff);
        let frame = Frame::new([2; 16], 0, compress_to_vec_zlib(&decoded, 6), true);
        let mut payload = vec![0];
        payload.extend(frame.encode());
        assert!(
            BatchObserver::default()
                .ingest(payload.into(), submission(11), &target(), &RollupConfig::default())
                .is_err()
        );
    }

    #[test]
    fn rejects_malformed_frames_instead_of_counting_batcher_activity() {
        let mut observer = BatchObserver::default();
        assert!(
            observer
                .ingest(
                    Bytes::from_static(&[0, 1, 2]),
                    submission(11),
                    &target(),
                    &RollupConfig::default()
                )
                .is_err()
        );
    }
}
