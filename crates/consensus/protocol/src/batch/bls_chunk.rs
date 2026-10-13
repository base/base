//! This module contains the development-only [`BlsChunkBatch`] type.

use alloc::vec::Vec;

use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Bytes, FixedBytes, bytes};
use alloy_rlp::{Decodable, Encodable, Header};
use base_common_consensus::BlsConstants;
use base_common_genesis::RollupConfig;
use tracing::warn;

use crate::{
    BatchDropReason, BatchValidationProvider, BatchValidity, BlsChunkError, L2BlockInfo,
    SingleBatch,
};

/// An authenticated chunk of consecutive single batches for the native BLS development chain.
///
/// Canonical native BLS transfers (type [`Self::TRANSFER_TX_TYPE`]) carry no signature, so a plain
/// [`SingleBatch`] cannot authorize them. A chunk carries the 48-byte aggregate of the per-block
/// sequencer witnesses next to the canonical block contents. Derivation verifies the aggregate
/// over the whole chunk before it releases any member.
///
/// Wire format after the batch type byte: `rlp([aggregate48_or_empty, [SingleBatch, ...]])`.
/// The aggregate is present exactly when at least one member carries a transfer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BlsChunkBatch {
    /// Aggregate BLS signature over every transfer in the chunk.
    pub aggregate: Option<FixedBytes<48>>,
    /// Consecutive single batches, each holding its block's canonical transaction bytes.
    pub batches: Vec<SingleBatch>,
}

impl BlsChunkBatch {
    /// Maximum number of blocks in one chunk.
    pub const MAX_BLOCKS: usize = 128;

    /// Maximum number of transfers in one chunk.
    pub const MAX_TRANSFERS: usize = 16_384;

    /// EIP-2718 type of a canonical native BLS transfer.
    pub const TRANSFER_TX_TYPE: u8 = BlsConstants::TRANSFER_TX_TYPE;

    /// Returns whether an encoded transaction is a canonical native BLS transfer.
    pub fn is_transfer(tx: &[u8]) -> bool {
        tx.first() == Some(&Self::TRANSFER_TX_TYPE)
    }

    /// Returns the number of native BLS transfers among encoded transactions.
    pub fn count_transfers(transactions: &[Bytes]) -> usize {
        transactions.iter().filter(|tx| Self::is_transfer(tx)).count()
    }

    /// Returns the number of native BLS transfers in the chunk.
    pub fn transfer_count(&self) -> usize {
        self.batches.iter().map(|batch| Self::count_transfers(&batch.transactions)).sum()
    }

    /// Returns the timestamp of the first member, or zero for an empty chunk.
    pub fn starting_timestamp(&self) -> u64 {
        self.batches.first().map_or(0, |batch| batch.timestamp)
    }

    /// Returns each member's transactions in chunk order, as passed to the chunk verifier.
    pub fn block_transactions(&self) -> Vec<Vec<Bytes>> {
        self.batches.iter().map(|batch| batch.transactions.clone()).collect()
    }

    /// Checks the chunk's structural bounds and aggregate presence.
    pub fn validate(&self) -> Result<(), BlsChunkError> {
        if self.batches.is_empty() {
            return Err(BlsChunkError::Empty);
        }
        if self.batches.len() > Self::MAX_BLOCKS {
            return Err(BlsChunkError::TooManyBlocks(self.batches.len()));
        }
        let transfers = self.transfer_count();
        if transfers > Self::MAX_TRANSFERS {
            return Err(BlsChunkError::TooManyTransfers(transfers));
        }
        match (transfers, self.aggregate) {
            (0, Some(_)) => Err(BlsChunkError::UnexpectedAggregate),
            (1.., None) => Err(BlsChunkError::MissingAggregate),
            _ => Ok(()),
        }
    }

    /// Encodes the chunk without the batch type byte.
    pub fn encode(&self, out: &mut dyn bytes::BufMut) -> Result<(), BlsChunkError> {
        self.validate()?;
        let aggregate = self.aggregate.as_ref().map_or(&[][..], |aggregate| aggregate.as_slice());
        let payload_length = aggregate.length() + self.batches.length();
        Header { list: true, payload_length }.encode(out);
        aggregate.encode(out);
        self.batches.encode(out);
        Ok(())
    }

    /// Decodes and validates a chunk without the batch type byte.
    ///
    /// The encoding must be exact, and members are counted while decoding so an oversized chunk
    /// is rejected before it is fully materialized.
    pub fn decode(buf: &mut &[u8]) -> Result<Self, BlsChunkError> {
        let mut payload = Header::decode_bytes(buf, true)?;
        if !buf.is_empty() {
            return Err(BlsChunkError::TrailingBytes);
        }

        let aggregate = match Header::decode_bytes(&mut payload, false)? {
            [] => None,
            bytes if bytes.len() == 48 => Some(FixedBytes::from_slice(bytes)),
            bytes => return Err(BlsChunkError::InvalidAggregateLength(bytes.len())),
        };

        let mut members = Header::decode_bytes(&mut payload, true)?;
        if !payload.is_empty() {
            return Err(BlsChunkError::TrailingBytes);
        }
        let mut batches = Vec::new();
        let mut transfers = 0;
        while !members.is_empty() {
            if batches.len() == Self::MAX_BLOCKS {
                return Err(BlsChunkError::TooManyBlocks(Self::MAX_BLOCKS + 1));
            }
            let batch = SingleBatch::decode(&mut members)?;
            transfers += Self::count_transfers(&batch.transactions);
            if transfers > Self::MAX_TRANSFERS {
                return Err(BlsChunkError::TooManyTransfers(transfers));
            }
            batches.push(batch);
        }

        let chunk = Self { aggregate, batches };
        chunk.validate()?;
        Ok(chunk)
    }

    /// Locates the chunk relative to the L2 safe head.
    ///
    /// Returns [`BatchValidity::Accept`] with the number of leading members that are already safe,
    /// [`BatchValidity::Past`] when no member extends the safe chain, or [`BatchValidity::Drop`]
    /// when the chunk can never apply. A chunk that starts before the safe head must build on a
    /// canonical block, and its overlapped members must match the canonical blocks exactly, so
    /// verifying the whole chunk at its first parent replays exactly the canonical prefix.
    ///
    /// Provider errors are returned unchanged so the caller can retry the same chunk.
    pub async fn check_prefix<BV: BatchValidationProvider>(
        &self,
        cfg: &RollupConfig,
        l2_safe_head: L2BlockInfo,
        fetcher: &mut BV,
    ) -> Result<(BatchValidity, usize), BV::Error> {
        let Some(first) = self.batches.first() else {
            return Ok((BatchValidity::Drop(BatchDropReason::BlsChunkInvalid), 0));
        };
        let safe_number = l2_safe_head.block_info.number;
        if first.parent_hash == l2_safe_head.block_info.hash {
            return Ok((BatchValidity::Accept, 0));
        }
        if first.timestamp > cfg.l2_block_timestamp(safe_number.saturating_add(1)) {
            return Ok((BatchValidity::Drop(BatchDropReason::FutureTimestampHolocene), 0));
        }

        // Find the canonical parent among blocks whose successor has the first member's timestamp.
        // A parent more than `MAX_BLOCKS` below the safe head leaves no member to release.
        let lowest = safe_number.saturating_sub(Self::MAX_BLOCKS as u64).max(cfg.genesis.l2.number);
        let mut parent_number = None;
        for number in (lowest..safe_number).rev() {
            let timestamp = cfg.l2_block_timestamp(number.saturating_add(1));
            if timestamp < first.timestamp {
                break;
            }
            if timestamp == first.timestamp
                && fetcher.l2_block_info_by_number(number).await?.block_info.hash
                    == first.parent_hash
            {
                parent_number = Some(number);
                break;
            }
        }
        let Some(parent_number) = parent_number else {
            return Ok((BatchValidity::Past, 0));
        };
        let overlap = (safe_number - parent_number) as usize;
        if overlap >= self.batches.len() {
            return Ok((BatchValidity::Past, 0));
        }

        for (index, member) in self.batches[..overlap].iter().enumerate() {
            let block = fetcher.block_by_number(parent_number + 1 + index as u64).await?;
            let deposits = block.body.transactions.iter().take_while(|tx| tx.is_deposit()).count();
            let canonical = &block.body.transactions[deposits..];
            if block.header.parent_hash != member.parent_hash
                || block.header.timestamp != member.timestamp
                || canonical.len() != member.transactions.len()
            {
                warn!(target: "batch_bls_chunk", index, "overlapped chunk member does not match");
                return Ok((BatchValidity::Drop(BatchDropReason::OverlappedTxCountMismatch), 0));
            }
            if canonical
                .iter()
                .zip(&member.transactions)
                .any(|(tx, raw)| tx.encoded_2718() != **raw)
            {
                warn!(target: "batch_bls_chunk", index, "overlapped chunk transaction does not match");
                return Ok((BatchValidity::Drop(BatchDropReason::OverlappedTxMismatch), 0));
            }
            match L2BlockInfo::from_block_and_genesis(&block, &cfg.genesis) {
                Ok(info) if info.l1_origin == member.epoch() => {}
                Ok(_) => {
                    return Ok((
                        BatchValidity::Drop(BatchDropReason::OverlappedL1OriginMismatch),
                        0,
                    ));
                }
                Err(_) => {
                    return Ok((
                        BatchValidity::Drop(BatchDropReason::L2BlockInfoExtractionFailed),
                        0,
                    ));
                }
            }
        }

        Ok((BatchValidity::Accept, overlap))
    }
}

#[cfg(test)]
mod tests {
    use alloc::{vec, vec::Vec};

    use alloy_consensus::{BlockBody, Header as BlockHeader, SignableTransaction, TxEip1559};
    use alloy_eips::BlockNumHash;
    use alloy_primitives::{Address, B256, Sealable, Signature, TxKind, U256, keccak256};
    use base_common_consensus::{BaseBlock, BaseTxEnvelope, TxBlsTransfer, TxDeposit};
    use base_common_genesis::ChainGenesis;
    use rstest::rstest;

    use super::*;
    use crate::{
        Batch, BatchDecodingError, BatchType, BlockInfo, L1BlockInfoBedrock, L1BlockInfoTx,
        test_utils::TestBatchValidator,
    };

    fn dev_config() -> RollupConfig {
        RollupConfig {
            l2_chain_id: BlsConstants::CHAIN_ID.into(),
            block_time: 2,
            genesis: ChainGenesis { l2_time: 100, ..Default::default() },
            ..Default::default()
        }
    }

    fn transfer(sender: u8, nonce: u64) -> BaseTxEnvelope {
        BaseTxEnvelope::BlsTransfer(
            TxBlsTransfer {
                chain_id: BlsConstants::CHAIN_ID,
                sender: Address::repeat_byte(sender),
                nonce,
                recipient: Address::repeat_byte(0xee),
                value: U256::from(1_000 + nonce),
                max_priority_fee_per_gas: 1,
                max_fee_per_gas: 2,
            }
            .seal_slow(),
        )
    }

    fn eip1559(nonce: u64) -> BaseTxEnvelope {
        BaseTxEnvelope::Eip1559(
            TxEip1559 {
                chain_id: BlsConstants::CHAIN_ID,
                nonce,
                gas_limit: 21_000,
                to: TxKind::Call(Address::repeat_byte(0x55)),
                ..Default::default()
            }
            .into_signed(Signature::test_signature()),
        )
    }

    fn raw(tx: &BaseTxEnvelope) -> Bytes {
        tx.encoded_2718().into()
    }

    fn member(transactions: Vec<Bytes>) -> SingleBatch {
        SingleBatch { transactions, ..Default::default() }
    }

    /// A chunk with `blocks` members and `transfers` transfers spread over them.
    fn chunk(blocks: usize, transfers: usize) -> BlsChunkBatch {
        let mut batches = vec![SingleBatch::default(); blocks];
        for index in 0..transfers {
            batches[index % blocks].transactions.push(raw(&transfer(1, index as u64)));
        }
        let aggregate = (transfers > 0).then(|| FixedBytes::repeat_byte(0xa1));
        BlsChunkBatch { aggregate, batches }
    }

    /// Encodes a chunk without validating it, as a malicious batcher could.
    fn encode_unchecked(aggregate: &[u8], batches: &Vec<SingleBatch>) -> Vec<u8> {
        let mut out = vec![BatchType::BLS_CHUNK];
        Header { list: true, payload_length: aggregate.length() + batches.length() }
            .encode(&mut out);
        aggregate.encode(&mut out);
        batches.encode(&mut out);
        out
    }

    fn decode(bytes: &[u8]) -> Result<Batch, BatchDecodingError> {
        Batch::decode(&mut &bytes[..], &dev_config())
    }

    #[test]
    fn roundtrip_preserves_canonical_transactions() {
        let txs = [eip1559(0), transfer(1, 0), transfer(2, 7), transfer(1, 1)];
        let batch = Batch::BlsChunk(BlsChunkBatch {
            aggregate: Some(FixedBytes::repeat_byte(0xa1)),
            batches: vec![
                SingleBatch {
                    parent_hash: B256::repeat_byte(1),
                    epoch_num: 3,
                    epoch_hash: B256::repeat_byte(2),
                    timestamp: 102,
                    transactions: txs[..2].iter().map(raw).collect(),
                },
                member(vec![]),
                member(txs[2..].iter().map(raw).collect()),
            ],
        });
        let mut out = Vec::new();
        batch.encode(&mut out).unwrap();
        assert_eq!(out[0], BatchType::BLS_CHUNK);

        let decoded = decode(&out).unwrap();
        assert_eq!(decoded, batch);
        let Batch::BlsChunk(decoded) = decoded else { unreachable!() };
        assert_eq!(decoded.transfer_count(), 3);
        // Intent hashes are the hashes of the exact canonical bytes the block committed to.
        let hashes: Vec<_> =
            decoded.batches.iter().flat_map(|b| &b.transactions).map(keccak256).collect();
        assert_eq!(hashes, txs.iter().map(BaseTxEnvelope::tx_hash).collect::<Vec<_>>());
    }

    #[test]
    fn chunk_type_is_rejected_off_dev_chain() {
        let mut out = Vec::new();
        Batch::BlsChunk(chunk(1, 1)).encode(&mut out).unwrap();
        let cfg = RollupConfig { l2_chain_id: 8453.into(), ..dev_config() };
        assert_eq!(
            Batch::decode(&mut out.as_slice(), &cfg),
            Err(BatchDecodingError::InvalidBatchType(BatchType::BLS_CHUNK))
        );
    }

    #[rstest]
    #[case::max_blocks(BlsChunkBatch::MAX_BLOCKS, 1, Ok(()))]
    #[case::one_block_too_many(
        BlsChunkBatch::MAX_BLOCKS + 1,
        1,
        Err(BlsChunkError::TooManyBlocks(BlsChunkBatch::MAX_BLOCKS + 1))
    )]
    #[case::max_transfers(32, BlsChunkBatch::MAX_TRANSFERS, Ok(()))]
    #[case::one_transfer_too_many(
        32,
        BlsChunkBatch::MAX_TRANSFERS + 1,
        Err(BlsChunkError::TooManyTransfers(BlsChunkBatch::MAX_TRANSFERS + 1))
    )]
    fn decode_enforces_bounds(
        #[case] blocks: usize,
        #[case] transfers: usize,
        #[case] expected: Result<(), BlsChunkError>,
    ) {
        let chunk = chunk(blocks, transfers);
        let aggregate = chunk.aggregate.unwrap_or_default();
        let bytes = encode_unchecked(aggregate.as_slice(), &chunk.batches);
        let decoded = decode(&bytes).map(|_| ()).map_err(|error| match error {
            BatchDecodingError::BlsChunkError(error) => error,
            other => panic!("unexpected error {other}"),
        });
        assert_eq!(decoded, expected);
        assert_eq!(chunk.validate(), expected);
    }

    #[rstest]
    #[case::missing_aggregate(&[][..], 1, BlsChunkError::MissingAggregate)]
    #[case::unexpected_aggregate(&[0xa1; 48][..], 0, BlsChunkError::UnexpectedAggregate)]
    #[case::short_aggregate(&[0xa1; 47][..], 1, BlsChunkError::InvalidAggregateLength(47))]
    #[case::long_aggregate(&[0xa1; 49][..], 1, BlsChunkError::InvalidAggregateLength(49))]
    fn decode_rejects_aggregate_mismatch(
        #[case] aggregate: &[u8],
        #[case] transfers: usize,
        #[case] expected: BlsChunkError,
    ) {
        let bytes = encode_unchecked(aggregate, &chunk(2, transfers).batches);
        assert_eq!(decode(&bytes), Err(BatchDecodingError::BlsChunkError(expected)));
    }

    #[test]
    fn decode_rejects_empty_and_trailing_bytes() {
        let bytes = encode_unchecked(&[], &Vec::new());
        assert_eq!(decode(&bytes), Err(BatchDecodingError::BlsChunkError(BlsChunkError::Empty)));

        let mut bytes = Vec::new();
        Batch::BlsChunk(chunk(2, 1)).encode(&mut bytes).unwrap();
        bytes.push(0);
        assert_eq!(
            decode(&bytes),
            Err(BatchDecodingError::BlsChunkError(BlsChunkError::TrailingBytes))
        );
    }

    #[test]
    fn encode_refuses_invalid_chunks() {
        let mut unauthenticated = chunk(2, 1);
        unauthenticated.aggregate = None;
        assert_eq!(
            Batch::BlsChunk(unauthenticated).encode(&mut Vec::new()),
            Err(crate::BatchEncodingError::BlsChunkError(BlsChunkError::MissingAggregate))
        );
    }

    /// A canonical chain of blocks `0..=6` and chunk members for blocks `3..=6`.
    struct Chain {
        cfg: RollupConfig,
        infos: Vec<L2BlockInfo>,
        blocks: Vec<BaseBlock>,
        members: Vec<SingleBatch>,
    }

    impl Chain {
        fn new() -> Self {
            let mut cfg = dev_config();
            let mut infos = Vec::new();
            let mut blocks = Vec::new();
            let mut members = Vec::new();
            let mut parent_hash = B256::ZERO;
            for number in 0..=6u64 {
                let epoch = BlockNumHash { number: 10 + number / 3, hash: B256::repeat_byte(0x10) };
                let info = L1BlockInfoBedrock::new(
                    epoch.number,
                    0,
                    0,
                    epoch.hash,
                    0,
                    Address::ZERO,
                    U256::ZERO,
                    U256::ZERO,
                );
                let deposit = BaseTxEnvelope::Deposit(
                    TxDeposit {
                        input: L1BlockInfoTx::Bedrock(info).encode_calldata(),
                        ..Default::default()
                    }
                    .seal_slow(),
                );
                let user = vec![eip1559(number), transfer(number as u8, number)];
                let block = BaseBlock {
                    header: BlockHeader {
                        number,
                        parent_hash,
                        timestamp: cfg.l2_block_timestamp(number),
                        ..Default::default()
                    },
                    body: BlockBody {
                        transactions: [vec![deposit], user.clone()].concat(),
                        ..Default::default()
                    },
                };
                let hash = block.header.hash_slow();
                if number == 0 {
                    cfg.genesis.l2 = BlockNumHash { number, hash };
                }
                infos.push(L2BlockInfo {
                    block_info: BlockInfo {
                        number,
                        hash,
                        parent_hash,
                        timestamp: block.header.timestamp,
                    },
                    l1_origin: epoch,
                    seq_num: 0,
                });
                if number >= 3 {
                    members.push(SingleBatch {
                        parent_hash,
                        epoch_num: epoch.number,
                        epoch_hash: epoch.hash,
                        timestamp: block.header.timestamp,
                        transactions: user.iter().map(raw).collect(),
                    });
                }
                parent_hash = hash;
                blocks.push(block);
            }
            Self { cfg, infos, blocks, members }
        }

        fn chunk(&self) -> BlsChunkBatch {
            BlsChunkBatch {
                aggregate: Some(FixedBytes::repeat_byte(0xa1)),
                batches: self.members.clone(),
            }
        }

        async fn check(&self, chunk: &BlsChunkBatch, safe: usize) -> (BatchValidity, usize) {
            let mut fetcher = TestBatchValidator::new(self.infos.clone(), self.blocks.clone());
            chunk.check_prefix(&self.cfg, self.infos[safe], &mut fetcher).await.unwrap()
        }
    }

    #[rstest]
    #[case::next_block(2, (BatchValidity::Accept, 0))]
    #[case::overlaps_one(3, (BatchValidity::Accept, 1))]
    #[case::overlaps_all_but_last(5, (BatchValidity::Accept, 3))]
    #[case::fully_safe(6, (BatchValidity::Past, 0))]
    #[case::starts_in_future(1, (BatchValidity::Drop(BatchDropReason::FutureTimestampHolocene), 0))]
    #[tokio::test]
    async fn check_prefix_locates_chunk(
        #[case] safe: usize,
        #[case] expected: (BatchValidity, usize),
    ) {
        let chain = Chain::new();
        assert_eq!(chain.check(&chain.chunk(), safe).await, expected);
    }

    #[rstest]
    #[case::altered_transfer(1, BatchDropReason::OverlappedTxMismatch)]
    #[case::reordered(2, BatchDropReason::OverlappedTxMismatch)]
    #[case::omitted(3, BatchDropReason::OverlappedTxCountMismatch)]
    #[case::wrong_epoch(4, BatchDropReason::OverlappedL1OriginMismatch)]
    #[tokio::test]
    async fn check_prefix_requires_exact_overlap(
        #[case] corruption: u8,
        #[case] expected: BatchDropReason,
    ) {
        let chain = Chain::new();
        let mut chunk = chain.chunk();
        let overlapped = &mut chunk.batches[1];
        match corruption {
            1 => overlapped.transactions[1] = raw(&transfer(4, 5)),
            2 => overlapped.transactions.reverse(),
            3 => {
                overlapped.transactions.pop();
            }
            _ => overlapped.epoch_num += 1,
        }
        assert_eq!(chain.check(&chunk, 5).await, (BatchValidity::Drop(expected), 0));
        // The same chunk is still acceptable where nothing overlaps the altered member.
        assert_eq!(chain.check(&chunk, 2).await, (BatchValidity::Accept, 0));
    }

    #[tokio::test]
    async fn check_prefix_treats_non_canonical_parent_as_past() {
        let chain = Chain::new();
        let mut chunk = chain.chunk();
        chunk.batches[0].parent_hash = B256::repeat_byte(0xbb);
        assert_eq!(chain.check(&chunk, 4).await, (BatchValidity::Past, 0));
    }
}
