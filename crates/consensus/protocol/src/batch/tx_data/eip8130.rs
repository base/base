//! This module contains the eip8130 transaction data type for a span batch.

use alloc::vec::Vec;

use alloy_primitives::{Address, Bytes, U256};
use alloy_rlp::{RlpDecodable, RlpEncodable};
use base_common_consensus::{AccountChangeEntry, Call, TxEip8130};

use crate::{SpanBatchError, SpanDecodingError};

/// The transaction data for an EIP-8130 transaction within a span batch.
#[derive(Debug, Clone, PartialEq, Eq, RlpEncodable, RlpDecodable)]
pub struct SpanBatchEip8130TransactionData {
    /// Sender address. `Address::ZERO` indicates EOA-mode sender recovery.
    pub from: Address,
    /// 2D nonce channel selector.
    pub nonce_key: U256,
    /// Optional expiry timestamp.
    pub expiry: u64,
    /// Maximum priority fee per gas.
    pub max_priority_fee_per_gas: U256,
    /// Maximum fee per gas.
    pub max_fee_per_gas: U256,
    /// Account creation and configuration changes.
    pub account_changes: Vec<AccountChangeEntry>,
    /// Phased call batches.
    pub calls: Vec<Vec<Call>>,
    /// Payer address. `Address::ZERO` indicates self-pay.
    pub payer: Address,
    /// Sender authentication payload.
    pub sender_auth: Bytes,
    /// Payer authentication payload.
    pub payer_auth: Bytes,
}

impl SpanBatchEip8130TransactionData {
    fn u256_to_u128_checked(value: U256) -> Result<u128, SpanBatchError> {
        let bytes = value.to_be_bytes::<32>();
        if bytes[..16].iter().any(|byte| *byte != 0) {
            return Err(SpanBatchError::Decoding(SpanDecodingError::InvalidTransactionData));
        }
        Ok(u128::from_be_bytes(
            bytes[16..]
                .try_into()
                .map_err(|_| SpanBatchError::Decoding(SpanDecodingError::InvalidTransactionData))?,
        ))
    }

    /// Converts [`SpanBatchEip8130TransactionData`] into a [`TxEip8130`].
    pub fn to_tx(&self, nonce: u64, gas: u64, chain_id: u64) -> Result<TxEip8130, SpanBatchError> {
        Ok(TxEip8130 {
            chain_id,
            from: (self.from != Address::ZERO).then_some(self.from),
            nonce_key: self.nonce_key,
            nonce_sequence: nonce,
            expiry: self.expiry,
            max_priority_fee_per_gas: Self::u256_to_u128_checked(self.max_priority_fee_per_gas)?,
            max_fee_per_gas: Self::u256_to_u128_checked(self.max_fee_per_gas)?,
            gas_limit: gas,
            account_changes: self.account_changes.clone(),
            calls: self.calls.clone(),
            payer: (self.payer != Address::ZERO).then_some(self.payer),
            sender_auth: self.sender_auth.clone(),
            payer_auth: self.payer_auth.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use alloc::{vec, vec::Vec};

    use alloy_rlp::{Decodable, Encodable};

    use super::*;
    use crate::SpanBatchTransactionData;

    #[test]
    fn encode_eip8130_tx_data_roundtrip() {
        let aa_tx = SpanBatchEip8130TransactionData {
            from: Address::repeat_byte(0xAA),
            nonce_key: U256::from(0xBB_u64),
            expiry: 123,
            max_priority_fee_per_gas: U256::from(0xCC_u64),
            max_fee_per_gas: U256::from(0xDD_u64),
            account_changes: vec![],
            calls: vec![vec![Call {
                to: Address::repeat_byte(0xEE),
                data: Bytes::from(vec![0x01, 0x02, 0x03]),
            }]],
            payer: Address::repeat_byte(0xFF),
            sender_auth: Bytes::from(vec![0x04, 0x05]),
            payer_auth: Bytes::from(vec![0x06, 0x07]),
        };

        let mut encoded_buf = Vec::new();
        SpanBatchTransactionData::Eip8130(aa_tx.clone()).encode(&mut encoded_buf);

        let decoded = SpanBatchTransactionData::decode(&mut encoded_buf.as_slice()).unwrap();
        let SpanBatchTransactionData::Eip8130(decoded_aa_tx) = decoded else {
            panic!("Expected SpanBatchEip8130TransactionData, got {decoded:?}");
        };

        assert_eq!(aa_tx, decoded_aa_tx);
    }

    #[test]
    fn to_tx_rejects_fee_overflow() {
        let aa_tx = SpanBatchEip8130TransactionData {
            from: Address::repeat_byte(0xAA),
            nonce_key: U256::ZERO,
            expiry: 0,
            max_priority_fee_per_gas: U256::from(1_u128) << 200,
            max_fee_per_gas: U256::from(1_u64),
            account_changes: vec![],
            calls: vec![],
            payer: Address::ZERO,
            sender_auth: Bytes::new(),
            payer_auth: Bytes::new(),
        };

        let result = aa_tx.to_tx(0, 21_000, 8453);
        assert!(matches!(
            result,
            Err(SpanBatchError::Decoding(SpanDecodingError::InvalidTransactionData))
        ));
    }
}
