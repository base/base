//! This module contains the top level span batch transaction data type.

use alloy_consensus::{Transaction, TxEnvelope, TxType};
use alloy_primitives::{Address, PrimitiveSignature as Signature, U256};
use alloy_rlp::{Bytes, Decodable, Encodable};
use op_alloy_consensus::OpTxEnvelope;

use crate::{
    SpanBatchEip1559TransactionData, SpanBatchEip2930TransactionData, SpanBatchError,
    SpanBatchLegacyTransactionData, SpanDecodingError,
};

/// The typed transaction data for a transaction within a span batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpanBatchTransactionData {
    /// Legacy transaction data.
    Legacy(SpanBatchLegacyTransactionData),
    /// EIP-2930 transaction data.
    Eip2930(SpanBatchEip2930TransactionData),
    /// EIP-1559 transaction data.
    Eip1559(SpanBatchEip1559TransactionData),
}

impl Encodable for SpanBatchTransactionData {
    fn encode(&self, out: &mut dyn alloy_rlp::BufMut) {
        match self {
            Self::Legacy(data) => {
                data.encode(out);
            }
            Self::Eip2930(data) => {
                out.put_u8(TxType::Eip2930 as u8);
                data.encode(out);
            }
            Self::Eip1559(data) => {
                out.put_u8(TxType::Eip1559 as u8);
                data.encode(out);
            }
        }
    }
}

impl Decodable for SpanBatchTransactionData {
    fn decode(r: &mut &[u8]) -> Result<Self, alloy_rlp::Error> {
        if !r.is_empty() && r[0] > 0x7F {
            // Legacy transaction
            return Ok(Self::Legacy(SpanBatchLegacyTransactionData::decode(r)?));
        }
        // Non-legacy transaction (EIP-2718 envelope encoding)
        Self::decode_typed(r)
    }
}

impl TryFrom<&TxEnvelope> for SpanBatchTransactionData {
    type Error = SpanBatchError;

    fn try_from(tx_envelope: &TxEnvelope) -> Result<Self, Self::Error> {
        match tx_envelope {
            TxEnvelope::Legacy(s) => {
                let s = s.tx();
                Ok(Self::Legacy(SpanBatchLegacyTransactionData {
                    value: s.value,
                    gas_price: U256::from(s.gas_price),
                    data: Bytes::from(s.input().to_vec()),
                }))
            }
            TxEnvelope::Eip2930(s) => {
                let s = s.tx();
                Ok(Self::Eip2930(SpanBatchEip2930TransactionData {
                    value: s.value,
                    gas_price: U256::from(s.gas_price),
                    data: Bytes::from(s.input().to_vec()),
                    access_list: s.access_list.clone(),
                }))
            }
            TxEnvelope::Eip1559(s) => {
                let s = s.tx();
                Ok(Self::Eip1559(SpanBatchEip1559TransactionData {
                    value: s.value,
                    max_fee_per_gas: U256::from(s.max_fee_per_gas),
                    max_priority_fee_per_gas: U256::from(s.max_priority_fee_per_gas),
                    data: Bytes::from(s.input().to_vec()),
                    access_list: s.access_list.clone(),
                }))
            }
            _ => Err(SpanBatchError::Decoding(SpanDecodingError::InvalidTransactionType)),
        }
    }
}

impl SpanBatchTransactionData {
    /// Returns the transaction type of the [SpanBatchTransactionData].
    pub const fn tx_type(&self) -> TxType {
        match self {
            Self::Legacy(_) => TxType::Legacy,
            Self::Eip2930(_) => TxType::Eip2930,
            Self::Eip1559(_) => TxType::Eip1559,
        }
    }

    /// Decodes a typed transaction into a [SpanBatchTransactionData] from a byte slice.
    pub fn decode_typed(b: &[u8]) -> Result<Self, alloy_rlp::Error> {
        if b.len() <= 1 {
            return Err(alloy_rlp::Error::Custom("Invalid transaction data"));
        }

        match b[0].try_into().map_err(|_| alloy_rlp::Error::Custom("Invalid tx type"))? {
            TxType::Eip2930 => {
                Ok(Self::Eip2930(SpanBatchEip2930TransactionData::decode(&mut &b[1..])?))
            }
            TxType::Eip1559 => {
                Ok(Self::Eip1559(SpanBatchEip1559TransactionData::decode(&mut &b[1..])?))
            }
            _ => Err(alloy_rlp::Error::Custom("Invalid transaction type")),
        }
    }

    /// Converts the [SpanBatchTransactionData] into a [OpTxEnvelope].
    pub fn to_enveloped_tx(
        &self,
        nonce: u64,
        gas: u64,
        to: Option<Address>,
        chain_id: u64,
        signature: Signature,
        is_protected: bool,
    ) -> Result<OpTxEnvelope, SpanBatchError> {
        match self {
            Self::Legacy(data) => {
                data.to_enveloped_tx(nonce, gas, to, chain_id, signature, is_protected)
            }
            Self::Eip2930(data) => data.to_enveloped_tx(nonce, gas, to, chain_id, signature),
            Self::Eip1559(data) => data.to_enveloped_tx(nonce, gas, to, chain_id, signature),
        }
    }
}
