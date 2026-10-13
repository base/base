//! This module contains the native BLS transaction data type for a span batch.

use alloy_consensus::{SignableTransaction, Signed};
use alloy_primitives::{Bytes, Signature};
use alloy_rlp::{RlpDecodable, RlpEncodable};
use base_common_consensus::TxBls;

/// The transaction data for a development-only native BLS transaction within a span batch.
///
/// The chain ID, nonce, gas limit and outer ECDSA signature travel in the shared span batch
/// columns. A BLS transaction has no `to` column entry: its call target is fixed, so the span
/// batch marks it with a set contract-creation bit, as for EIP-8130.
#[derive(Debug, Clone, PartialEq, Eq, RlpEncodable, RlpDecodable)]
pub struct SpanBatchBlsTransactionData {
    /// Maximum priority fee per gas.
    pub max_priority_fee_per_gas: u128,
    /// Maximum fee per gas.
    pub max_fee_per_gas: u128,
    /// Encoded BLS payload.
    pub data: Bytes,
}

impl SpanBatchBlsTransactionData {
    /// Converts [`SpanBatchBlsTransactionData`] into a signed [`TxBls`].
    pub fn to_signed_tx(
        &self,
        nonce: u64,
        gas: u64,
        chain_id: u64,
        signature: Signature,
    ) -> Signed<TxBls> {
        TxBls {
            chain_id,
            nonce,
            max_priority_fee_per_gas: self.max_priority_fee_per_gas,
            max_fee_per_gas: self.max_fee_per_gas,
            gas_limit: gas,
            input: self.data.clone(),
        }
        .into_signed(signature)
    }
}

impl From<&TxBls> for SpanBatchBlsTransactionData {
    fn from(tx: &TxBls) -> Self {
        Self {
            max_priority_fee_per_gas: tx.max_priority_fee_per_gas,
            max_fee_per_gas: tx.max_fee_per_gas,
            data: tx.input.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use alloy_rlp::{Decodable, Encodable};
    use base_common_consensus::OpTxType;

    use super::*;
    use crate::SpanBatchTransactionData;

    #[test]
    fn encode_bls_tx_data_roundtrip() {
        let data = SpanBatchBlsTransactionData {
            max_priority_fee_per_gas: 0xdd,
            max_fee_per_gas: u128::MAX,
            data: Bytes::from(alloc::vec![0x01, 0x02, 0x03]),
        };
        let mut encoded = Vec::new();
        SpanBatchTransactionData::Bls(data.clone()).encode(&mut encoded);
        assert_eq!(encoded[0], OpTxType::Bls as u8);

        let decoded = SpanBatchTransactionData::decode(&mut encoded.as_slice()).unwrap();
        assert_eq!(decoded, SpanBatchTransactionData::Bls(data));
    }
}
