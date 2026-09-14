//! Canonical EIP-2718 transaction decoding.

use alloy_eips::{
    Decodable2718, Encodable2718,
    eip2718::{Eip2718Error, Eip2718Result},
};

/// Decodes an EIP-2718 transaction and requires that `bytes` is its canonical encoding.
///
/// `decode_2718_exact` accepts inputs that re-encode differently, including a typed transaction
/// body without its type byte and a legacy transaction with a `0x00` type byte. Consensus inputs
/// must retain their wire encoding, so non-round-tripping transactions are rejected.
pub fn decode_2718_canonical<T: Decodable2718 + Encodable2718>(bytes: &[u8]) -> Eip2718Result<T> {
    let transaction = T::decode_2718_exact(bytes)?;

    if transaction.encode_2718_len() != bytes.len() || transaction.encoded_2718() != bytes {
        return Err(Eip2718Error::RlpError(alloy_rlp::Error::Custom(
            "non-canonical transaction encoding",
        )));
    }

    Ok(transaction)
}

#[cfg(test)]
mod tests {
    use alloy_consensus::{SignableTransaction, TxEip1559, TxEip2930, TxEip7702, TxLegacy};
    use alloy_primitives::{Address, B256, Bytes, Signature, TxKind, U256};

    use super::*;
    use crate::{BaseTxEnvelope, Eip8130Signed, TxDeposit, TxEip8130};

    fn canonical_transactions() -> Vec<BaseTxEnvelope> {
        let signature = Signature::test_signature();
        let to = Address::repeat_byte(0x11);

        vec![
            TxLegacy {
                chain_id: Some(8453),
                nonce: 1,
                gas_price: 2,
                gas_limit: 21_000,
                to: to.into(),
                value: U256::from(1u64),
                input: Bytes::new(),
            }
            .into_signed(signature)
            .into(),
            TxEip2930 {
                chain_id: 8453,
                nonce: 1,
                gas_price: 2,
                gas_limit: 21_000,
                to: to.into(),
                value: U256::from(1u64),
                access_list: Default::default(),
                input: Bytes::new(),
            }
            .into_signed(signature)
            .into(),
            TxEip1559 {
                chain_id: 8453,
                nonce: 1,
                gas_limit: 21_000,
                max_fee_per_gas: 2,
                max_priority_fee_per_gas: 1,
                to: to.into(),
                value: U256::from(1u64),
                access_list: Default::default(),
                input: Bytes::new(),
            }
            .into_signed(signature)
            .into(),
            TxEip7702 {
                chain_id: 8453,
                nonce: 1,
                gas_limit: 21_000,
                max_fee_per_gas: 2,
                max_priority_fee_per_gas: 1,
                to,
                value: U256::from(1u64),
                access_list: Default::default(),
                authorization_list: vec![],
                input: Bytes::new(),
            }
            .into_signed(signature)
            .into(),
            TxDeposit {
                source_hash: B256::with_last_byte(2),
                from: Address::repeat_byte(0x42),
                to: TxKind::Call(to),
                mint: 1,
                value: U256::from(1u64),
                gas_limit: 50_000,
                is_system_transaction: false,
                input: Bytes::new(),
            }
            .into(),
            BaseTxEnvelope::Eip8130(Eip8130Signed::new(
                TxEip8130 {
                    chain_id: 8453,
                    sender: Some(Address::repeat_byte(0x42)),
                    nonce_key: U256::from(1u64),
                    nonce_sequence: 1,
                    valid_after: 0,
                    valid_before: 1,
                    max_priority_fee_per_gas: 1,
                    max_fee_per_gas: 2,
                    gas_limit: 50_000,
                    account_changes: vec![],
                    calls: vec![],
                    metadata: Bytes::new(),
                    payer: None,
                },
                Bytes::from_static(&[0xde, 0xad, 0xbe, 0xef]),
                Bytes::new(),
            )),
        ]
    }

    #[test]
    fn accepts_canonical_encodings() {
        for transaction in canonical_transactions() {
            let encoded = transaction.encoded_2718();
            let decoded = decode_2718_canonical::<BaseTxEnvelope>(&encoded).unwrap();

            assert_eq!(decoded, transaction);
            assert_eq!(decoded.encoded_2718(), encoded);
        }
    }

    #[test]
    fn rejects_typed_body_without_type_byte() {
        let encoded = TxEip1559 {
            chain_id: 8453,
            nonce: 1,
            gas_limit: 21_000,
            max_fee_per_gas: 2,
            max_priority_fee_per_gas: 1,
            to: Address::ZERO.into(),
            value: U256::ZERO,
            access_list: Default::default(),
            input: Bytes::new(),
        }
        .into_signed(Signature::test_signature())
        .encoded_2718();

        assert_eq!(encoded[0], 0x02);
        assert!(decode_2718_canonical::<BaseTxEnvelope>(&encoded).is_ok());
        assert!(decode_2718_canonical::<BaseTxEnvelope>(&encoded[1..]).is_err());
    }

    #[test]
    fn rejects_type_tagged_legacy_transaction() {
        let encoded = TxLegacy {
            chain_id: Some(8453),
            nonce: 1,
            gas_price: 2,
            gas_limit: 21_000,
            to: Address::ZERO.into(),
            value: U256::ZERO,
            input: Bytes::new(),
        }
        .into_signed(Signature::test_signature())
        .encoded_2718();
        let mut tagged = vec![0x00];
        tagged.extend_from_slice(&encoded);

        assert!(decode_2718_canonical::<BaseTxEnvelope>(&encoded).is_ok());
        assert!(decode_2718_canonical::<BaseTxEnvelope>(&tagged).is_err());
    }
}
