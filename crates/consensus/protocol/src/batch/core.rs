//! Module containing the core [`Batch`] enum.

use alloy_primitives::bytes;
use alloy_rlp::{Buf, Decodable, Encodable};
use base_common_consensus::BlsConstants;
use base_common_genesis::RollupConfig;

use crate::{
    BatchDecodingError, BatchEncodingError, BatchType, BlsChunkBatch, RawSpanBatch, SingleBatch,
    SpanBatch,
};

/// A Batch.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::large_enum_variant)]
pub enum Batch {
    /// A single batch
    Single(SingleBatch),
    /// Span Batches
    Span(SpanBatch),
    /// Native BLS chunk batches, development chain only
    BlsChunk(BlsChunkBatch),
}

impl core::fmt::Display for Batch {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Single(_) => write!(f, "single"),
            Self::Span(_) => write!(f, "span"),
            Self::BlsChunk(_) => write!(f, "bls_chunk"),
        }
    }
}

impl Batch {
    /// Returns the timestamp for the batch.
    pub fn timestamp(&self) -> u64 {
        match self {
            Self::Single(sb) => sb.timestamp,
            Self::Span(sb) => sb.starting_timestamp(),
            Self::BlsChunk(chunk) => chunk.starting_timestamp(),
        }
    }

    /// Attempts to decode a batch from a reader.
    pub fn decode(r: &mut &[u8], cfg: &RollupConfig) -> Result<Self, BatchDecodingError> {
        if r.is_empty() {
            return Err(BatchDecodingError::EmptyBuffer);
        }

        // Read the batch type
        let batch_type = BatchType::try_from(r[0])?;
        r.advance(1);

        match batch_type {
            BatchType::Single => {
                let single_batch =
                    SingleBatch::decode(r).map_err(BatchDecodingError::AlloyRlpError)?;
                Ok(Self::Single(single_batch))
            }
            BatchType::Span => {
                let mut raw_span_batch = RawSpanBatch::decode(r)?;
                let span_batch = raw_span_batch
                    .derive(cfg.block_time, cfg.genesis.l2_time, cfg.l2_chain_id.id())
                    .map_err(BatchDecodingError::SpanBatchError)?;
                Ok(Self::Span(span_batch))
            }
            // Other chains keep rejecting the type byte exactly as before chunks existed.
            BatchType::BlsChunk if cfg.l2_chain_id.id() != BlsConstants::CHAIN_ID => {
                Err(BatchDecodingError::InvalidBatchType(BatchType::BLS_CHUNK))
            }
            BatchType::BlsChunk => Ok(Self::BlsChunk(BlsChunkBatch::decode(r)?)),
        }
    }

    /// Attempts to encode the batch to a writer.
    pub fn encode(&self, out: &mut dyn bytes::BufMut) -> Result<(), BatchEncodingError> {
        match self {
            Self::Single(sb) => {
                out.put_u8(BatchType::Single as u8);
                sb.encode(out);
            }
            Self::Span(sb) => {
                out.put_u8(BatchType::Span as u8);
                sb.encode(out).map_err(BatchEncodingError::SpanBatchError)?;
            }
            Self::BlsChunk(chunk) => {
                out.put_u8(BatchType::BlsChunk as u8);
                chunk.encode(out)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use alloc::{vec, vec::Vec};

    use alloy_consensus::{SignableTransaction, Signed, TxEip2930, TxEnvelope};
    use alloy_eips::eip2718::{Decodable2718, Encodable2718};
    use alloy_primitives::{Bytes, Signature, TxKind, U256, address, hex, keccak256};
    use base_common_consensus::{BaseTxEnvelope, BlsConstants, TxBls};

    use super::*;
    use crate::{SpanBatchElement, SpanBatchError, SpanBatchTransactions};

    #[test]
    fn test_single_batch_encode_decode() {
        let mut out = Vec::new();
        let batch = Batch::Single(SingleBatch::default());
        batch.encode(&mut out).unwrap();
        let decoded = Batch::decode(&mut out.as_slice(), &RollupConfig::default()).unwrap();
        assert_eq!(batch, decoded);
    }

    #[test]
    fn test_span_batch_encode_decode() {
        let sig = Signature::test_signature();
        let to = address!("0123456789012345678901234567890123456789");
        let tx = TxEnvelope::Eip2930(Signed::new_unchecked(
            TxEip2930 { to: TxKind::Call(to), chain_id: 1, ..Default::default() },
            sig,
            Default::default(),
        ));
        let mut span_batch_txs = SpanBatchTransactions::default();
        let mut buf = vec![];
        tx.encode(&mut buf);
        let txs = vec![Bytes::from(buf)];
        let chain_id = 1;
        span_batch_txs.add_txs(txs, chain_id).unwrap();

        let mut out = Vec::new();
        let batch = Batch::Span(SpanBatch {
            block_tx_counts: vec![1],
            batches: vec![SpanBatchElement::default()],
            txs: span_batch_txs,
            ..Default::default()
        });
        batch.encode(&mut out).unwrap();
        let decoded = Batch::decode(&mut out.as_slice(), &RollupConfig::default()).unwrap();
        assert_eq!(Batch::Span(SpanBatch {
            batches: vec![SpanBatchElement {
                transactions: vec![hex!("01f85f808080809401234567890123456789012345678901234567898080c080a0840cfc572845f5786e702984c2a582528cad4b49b2a10b9db1be7fca90058565a025e7109ceb98168d95b09b18bbf6b685130e0562f233877d492b94eee0c5b6d1").into()],
                ..Default::default()
            }],
            txs: SpanBatchTransactions::default(),
            ..Default::default()
        }), decoded);
    }

    #[test]
    fn test_bls_tx_single_and_span_batch_roundtrip() {
        let mut input = vec![BlsConstants::KIND_REGISTRATION, 0xa1];
        input.extend_from_slice(&[0x11; 95]);
        input.push(0x81);
        input.extend_from_slice(&[0x11; 47]);
        let tx = TxBls {
            chain_id: BlsConstants::CHAIN_ID,
            nonce: 9,
            max_priority_fee_per_gas: 1,
            max_fee_per_gas: 2,
            gas_limit: 80_000,
            input: input.into(),
        };
        let raw: Bytes = BaseTxEnvelope::Bls(tx.into_signed(Signature::new(
            U256::from(5u64),
            U256::from(6u64),
            true,
        )))
        .encoded_2718()
        .into();
        let cfg = RollupConfig {
            l2_chain_id: BlsConstants::CHAIN_ID.into(),
            block_time: 2,
            ..Default::default()
        };

        let single =
            Batch::Single(SingleBatch { transactions: vec![raw.clone()], ..Default::default() });
        let mut out = Vec::new();
        single.encode(&mut out).unwrap();
        assert_eq!(Batch::decode(&mut out.as_slice(), &cfg).unwrap(), single);

        let mut txs = SpanBatchTransactions::default();
        txs.add_txs(vec![raw.clone()], BlsConstants::CHAIN_ID).unwrap();
        let span = Batch::Span(SpanBatch {
            block_tx_counts: vec![1],
            batches: vec![SpanBatchElement::default()],
            chain_id: BlsConstants::CHAIN_ID,
            txs,
            ..Default::default()
        });
        let mut out = Vec::new();
        span.encode(&mut out).unwrap();
        let Batch::Span(decoded) = Batch::decode(&mut out.as_slice(), &cfg).unwrap() else {
            panic!("expected span batch");
        };
        let rebuilt = &decoded.batches[0].transactions[0];
        assert_eq!(rebuilt, &raw);
        let envelope = BaseTxEnvelope::decode_2718(&mut rebuilt.as_ref()).unwrap();
        assert!(envelope.is_bls());
        assert_eq!(envelope.tx_hash(), keccak256(&raw));
    }

    #[test]
    fn test_empty_span_batch() {
        let mut out = Vec::new();
        let batch = Batch::Span(SpanBatch::default());
        // Fails to even encode an empty span batch - decoding will do the same
        let err = batch.encode(&mut out).unwrap_err();
        assert_eq!(BatchEncodingError::SpanBatchError(SpanBatchError::EmptySpanBatch), err);
    }
}
