//! This module contains the top level span batch transaction data type.

use alloy_consensus::Transaction;
use alloy_primitives::{Address, Signature, U256};
use alloy_rlp::{Bytes, Decodable, Encodable};
use base_common_consensus::{BaseTxEnvelope, OpTxType};

use crate::{
    SpanBatchEip1559TransactionData, SpanBatchEip2930TransactionData,
    SpanBatchEip7702TransactionData, SpanBatchEip8130TransactionData, SpanBatchError,
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
    /// EIP-7702 transaction data.
    Eip7702(SpanBatchEip7702TransactionData),
    /// EIP-8130 transaction data.
    Eip8130(SpanBatchEip8130TransactionData),
}

impl Encodable for SpanBatchTransactionData {
    fn encode(&self, out: &mut dyn alloy_rlp::BufMut) {
        match self {
            Self::Legacy(data) => {
                data.encode(out);
            }
            Self::Eip2930(data) => {
                out.put_u8(OpTxType::Eip2930 as u8);
                data.encode(out);
            }
            Self::Eip1559(data) => {
                out.put_u8(OpTxType::Eip1559 as u8);
                data.encode(out);
            }
            Self::Eip7702(data) => {
                out.put_u8(OpTxType::Eip7702 as u8);
                data.encode(out);
            }
            Self::Eip8130(data) => {
                out.put_u8(OpTxType::Eip8130 as u8);
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

impl TryFrom<&BaseTxEnvelope> for SpanBatchTransactionData {
    type Error = SpanBatchError;

    fn try_from(tx_envelope: &BaseTxEnvelope) -> Result<Self, Self::Error> {
        match tx_envelope {
            BaseTxEnvelope::Legacy(s) => {
                let s = s.tx();
                Ok(Self::Legacy(SpanBatchLegacyTransactionData {
                    value: s.value,
                    gas_price: U256::from(s.gas_price),
                    data: Bytes::from(s.input().to_vec()),
                }))
            }
            BaseTxEnvelope::Eip2930(s) => {
                let s = s.tx();
                Ok(Self::Eip2930(SpanBatchEip2930TransactionData {
                    value: s.value,
                    gas_price: U256::from(s.gas_price),
                    data: Bytes::from(s.input().to_vec()),
                    access_list: s.access_list.clone(),
                }))
            }
            BaseTxEnvelope::Eip1559(s) => {
                let s = s.tx();
                Ok(Self::Eip1559(SpanBatchEip1559TransactionData {
                    value: s.value,
                    max_fee_per_gas: U256::from(s.max_fee_per_gas),
                    max_priority_fee_per_gas: U256::from(s.max_priority_fee_per_gas),
                    data: Bytes::from(s.input().to_vec()),
                    access_list: s.access_list.clone(),
                }))
            }
            BaseTxEnvelope::Eip7702(s) => {
                let s = s.tx();
                Ok(Self::Eip7702(SpanBatchEip7702TransactionData {
                    value: s.value,
                    max_fee_per_gas: U256::from(s.max_fee_per_gas),
                    max_priority_fee_per_gas: U256::from(s.max_priority_fee_per_gas),
                    data: Bytes::from(s.input().to_vec()),
                    access_list: s.access_list.clone(),
                    authorization_list: s.authorization_list.clone(),
                }))
            }
            BaseTxEnvelope::Eip8130(signed) => {
                let tx = signed.tx();
                Ok(Self::Eip8130(SpanBatchEip8130TransactionData {
                    sender: tx.sender,
                    nonce_key: tx.nonce_key,
                    expiry: tx.expiry,
                    max_priority_fee_per_gas: tx.max_priority_fee_per_gas,
                    max_fee_per_gas: tx.max_fee_per_gas,
                    payer: tx.payer,
                    account_changes: tx.account_changes.clone(),
                    calls: tx.calls.clone(),
                    metadata: tx.metadata.clone(),
                    sender_authenticator: SpanBatchEip8130TransactionData::split_auth(
                        signed.sender_auth(),
                        tx.sender.is_some(),
                    )?
                    .0,
                    payer_authenticator: SpanBatchEip8130TransactionData::split_auth(
                        signed.payer_auth(),
                        tx.payer.is_some(),
                    )?
                    .0,
                }))
            }
            BaseTxEnvelope::Deposit(_) => {
                Err(SpanBatchError::Decoding(SpanDecodingError::InvalidTransactionType))
            }
        }
    }
}

impl SpanBatchTransactionData {
    /// Returns the transaction type of the [`SpanBatchTransactionData`].
    pub const fn tx_type(&self) -> OpTxType {
        match self {
            Self::Legacy(_) => OpTxType::Legacy,
            Self::Eip2930(_) => OpTxType::Eip2930,
            Self::Eip1559(_) => OpTxType::Eip1559,
            Self::Eip7702(_) => OpTxType::Eip7702,
            Self::Eip8130(_) => OpTxType::Eip8130,
        }
    }

    /// Decodes a typed transaction into a [`SpanBatchTransactionData`] from a byte slice.
    pub fn decode_typed(b: &[u8]) -> Result<Self, alloy_rlp::Error> {
        if b.len() <= 1 {
            return Err(alloy_rlp::Error::Custom("Invalid transaction data"));
        }

        match b[0].try_into().map_err(|_| alloy_rlp::Error::Custom("Invalid tx type"))? {
            OpTxType::Eip2930 => {
                Ok(Self::Eip2930(SpanBatchEip2930TransactionData::decode(&mut &b[1..])?))
            }
            OpTxType::Eip1559 => {
                Ok(Self::Eip1559(SpanBatchEip1559TransactionData::decode(&mut &b[1..])?))
            }
            OpTxType::Eip7702 => {
                Ok(Self::Eip7702(SpanBatchEip7702TransactionData::decode(&mut &b[1..])?))
            }
            OpTxType::Eip8130 => {
                Ok(Self::Eip8130(SpanBatchEip8130TransactionData::decode(&mut &b[1..])?))
            }
            _ => Err(alloy_rlp::Error::Custom("Invalid transaction type")),
        }
    }

    /// Converts the [`SpanBatchTransactionData`] into a signed transaction as
    /// [`BaseTxEnvelope`].
    pub fn to_signed_tx(
        &self,
        nonce: u64,
        gas: u64,
        to: Option<Address>,
        chain_id: u64,
        signature: Signature,
        is_protected: bool,
    ) -> Result<BaseTxEnvelope, SpanBatchError> {
        Ok(match self {
            Self::Legacy(data) => BaseTxEnvelope::Legacy(data.to_signed_tx(
                nonce,
                gas,
                to,
                chain_id,
                signature,
                is_protected,
            )?),
            Self::Eip2930(data) => {
                BaseTxEnvelope::Eip2930(data.to_signed_tx(nonce, gas, to, chain_id, signature)?)
            }
            Self::Eip1559(data) => {
                BaseTxEnvelope::Eip1559(data.to_signed_tx(nonce, gas, to, chain_id, signature)?)
            }
            Self::Eip7702(data) => {
                let Some(addr) = to else {
                    return Err(SpanBatchError::Decoding(
                        SpanDecodingError::InvalidTransactionData,
                    ));
                };
                BaseTxEnvelope::Eip7702(data.to_signed_tx(nonce, gas, addr, chain_id, signature)?)
            }
            // EIP-8130 transactions are reconstructed through their dedicated path in
            // `full_txs`, which supplies the auth proofs carried in the trailing column.
            Self::Eip8130(_) => {
                return Err(SpanBatchError::Decoding(SpanDecodingError::InvalidTransactionType));
            }
        })
    }
}
