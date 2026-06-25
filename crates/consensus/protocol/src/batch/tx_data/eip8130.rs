//! This module contains the eip8130 transaction data type for a span batch.

use alloc::vec::Vec;

use alloy_primitives::{Address, Bytes, U256};
use alloy_rlp::{BufMut, Decodable, Encodable, Header};
use base_common_consensus::{AccountChange, Call, Eip8130Signed, TxEip8130};

use crate::{Channel, SpanBatchError, SpanDecodingError};

/// The low-entropy remainder of an EIP-8130 transaction within a span batch.
///
/// The chain id is dropped from the wire and reinjected from the batch chain id;
/// the nonce sequence and gas limit are carried in the shared nonce and gas
/// columns; the high-entropy authentication proofs are carried in a separate
/// trailing column. The two authenticator fields hold the leading 20-byte
/// account address on the configured-actor path and are empty on the EOA path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpanBatchEip8130TransactionData {
    /// Explicit sender account address, or `None` for the EOA path.
    pub sender: Option<Address>,
    /// High bits of the compound nonce.
    pub nonce_key: U256,
    /// Unix-seconds expiry timestamp; `0` means no expiry.
    pub expiry: u64,
    /// Max priority fee per gas (tip).
    pub max_priority_fee_per_gas: u128,
    /// Max total fee per gas.
    pub max_fee_per_gas: u128,
    /// Optional explicit payer; `None` means the resolved sender pays gas.
    pub payer: Option<Address>,
    /// Account-mutation entries applied before calls execute.
    pub account_changes: Vec<AccountChange>,
    /// Calls dispatched by the protocol, grouped into phases.
    pub calls: Vec<Vec<Call>>,
    /// Opaque attribution bytes.
    pub metadata: Bytes,
    /// Sender authenticator (leading account address on the configured path,
    /// empty on the EOA path).
    pub sender_authenticator: Bytes,
    /// Payer authenticator (leading account address on the configured path,
    /// empty on the EOA path).
    pub payer_authenticator: Bytes,
}

impl SpanBatchEip8130TransactionData {
    /// Fail-fast upper bound, in bytes, on a single EIP-8130 auth proof.
    ///
    /// A single proof cannot exceed the byte budget of the channel that carries
    /// the whole span batch, so it is derived from [`Channel::MAX_RLP_BYTES`].
    /// The genuine allocation bound is the subsequent `r.len() < n` check in the
    /// decoder; this constant only rejects an obviously corrupt length prefix
    /// before any copy is attempted.
    pub const MAX_AUTH_PROOF_BYTES: u64 = Channel::MAX_RLP_BYTES;

    /// Encodes an `Option<Address>` as a zero-length byte string when `None` and
    /// a 20-byte string when `Some`.
    fn encode_address_opt(addr: &Option<Address>, out: &mut dyn BufMut) {
        match addr {
            None => Bytes::new().encode(out),
            Some(a) => Bytes::copy_from_slice(a.as_slice()).encode(out),
        }
    }

    /// Length contribution of an `Option<Address>` under [`Self::encode_address_opt`].
    const fn address_opt_encoded_length(addr: &Option<Address>) -> usize {
        match addr {
            None => 1,
            Some(_) => 21,
        }
    }

    /// Decodes the [`Self::encode_address_opt`] wire format.
    fn decode_address_opt(buf: &mut &[u8]) -> alloy_rlp::Result<Option<Address>> {
        let raw = Bytes::decode(buf)?;
        match raw.len() {
            0 => Ok(None),
            20 => Ok(Some(Address::from_slice(&raw))),
            _ => Err(alloy_rlp::Error::Custom("invalid Option<Address> length")),
        }
    }

    fn rlp_encoded_fields_length(&self) -> usize {
        Self::address_opt_encoded_length(&self.sender)
            + self.nonce_key.length()
            + self.expiry.length()
            + self.max_priority_fee_per_gas.length()
            + self.max_fee_per_gas.length()
            + Self::address_opt_encoded_length(&self.payer)
            + self.account_changes.length()
            + self.calls.length()
            + self.metadata.length()
            + self.sender_authenticator.length()
            + self.payer_authenticator.length()
    }

    /// Splits an authentication blob into its authenticator and proof parts.
    ///
    /// Inverse of `join_auth`. On the EOA path (`configured` is false)
    /// the authenticator is empty and the whole blob is the proof. On the
    /// configured-actor path the blob is `authenticator(20) || proof`; a
    /// configured blob shorter than 20 bytes is rejected.
    pub fn split_auth(blob: &Bytes, configured: bool) -> Result<(Bytes, Bytes), SpanBatchError> {
        if !configured {
            return Ok((Bytes::new(), blob.clone()));
        }
        if blob.len() < 20 {
            return Err(SpanBatchError::Decoding(SpanDecodingError::InvalidAuthData));
        }
        Ok((blob.slice(..20), blob.slice(20..)))
    }

    /// Reassembles an authentication blob from its authenticator and proof parts.
    ///
    /// Inverse of `split_auth`.
    fn join_auth(authenticator: &Bytes, proof: &Bytes, configured: bool) -> Bytes {
        if !configured {
            proof.clone()
        } else if proof.is_empty() {
            authenticator.clone()
        } else {
            let mut out = Vec::with_capacity(authenticator.len() + proof.len());
            out.extend_from_slice(authenticator);
            out.extend_from_slice(proof);
            Bytes::from(out)
        }
    }

    /// Reconstructs the signed [`Eip8130Signed`] from the remainder, the shared
    /// columns, and the trailing-column auth proofs.
    pub fn to_tx(
        &self,
        chain_id: u64,
        nonce: u64,
        gas: u64,
        sender_proof: Bytes,
        payer_proof: Bytes,
    ) -> Eip8130Signed {
        let sender_auth =
            Self::join_auth(&self.sender_authenticator, &sender_proof, self.sender.is_some());
        let payer_auth =
            Self::join_auth(&self.payer_authenticator, &payer_proof, self.payer.is_some());
        let tx = TxEip8130 {
            chain_id,
            sender: self.sender,
            nonce_key: self.nonce_key,
            nonce_sequence: nonce,
            expiry: self.expiry,
            max_priority_fee_per_gas: self.max_priority_fee_per_gas,
            max_fee_per_gas: self.max_fee_per_gas,
            gas_limit: gas,
            account_changes: self.account_changes.clone(),
            calls: self.calls.clone(),
            metadata: self.metadata.clone(),
            payer: self.payer,
        };
        Eip8130Signed::new(tx, sender_auth, payer_auth)
    }
}

impl Encodable for SpanBatchEip8130TransactionData {
    fn encode(&self, out: &mut dyn BufMut) {
        Header { list: true, payload_length: self.rlp_encoded_fields_length() }.encode(out);
        Self::encode_address_opt(&self.sender, out);
        self.nonce_key.encode(out);
        self.expiry.encode(out);
        self.max_priority_fee_per_gas.encode(out);
        self.max_fee_per_gas.encode(out);
        Self::encode_address_opt(&self.payer, out);
        self.account_changes.encode(out);
        self.calls.encode(out);
        self.metadata.encode(out);
        self.sender_authenticator.encode(out);
        self.payer_authenticator.encode(out);
    }
}

impl Decodable for SpanBatchEip8130TransactionData {
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        let header = Header::decode(buf)?;
        if !header.list {
            return Err(alloy_rlp::Error::UnexpectedString);
        }
        let started = buf.len();
        let this = Self {
            sender: Self::decode_address_opt(buf)?,
            nonce_key: Decodable::decode(buf)?,
            expiry: Decodable::decode(buf)?,
            max_priority_fee_per_gas: Decodable::decode(buf)?,
            max_fee_per_gas: Decodable::decode(buf)?,
            payer: Self::decode_address_opt(buf)?,
            account_changes: Decodable::decode(buf)?,
            calls: Decodable::decode(buf)?,
            metadata: Decodable::decode(buf)?,
            sender_authenticator: Decodable::decode(buf)?,
            payer_authenticator: Decodable::decode(buf)?,
        };
        let consumed = started - buf.len();
        if consumed != header.payload_length {
            return Err(alloy_rlp::Error::ListLengthMismatch {
                expected: header.payload_length,
                got: consumed,
            });
        }
        Ok(this)
    }
}

#[cfg(test)]
mod tests {
    use alloc::{vec, vec::Vec};

    use alloy_primitives::{address, bytes};
    use base_common_consensus::Delegation;

    use super::*;
    use crate::SpanBatchTransactionData;

    #[test]
    fn encode_eip8130_tx_data_roundtrip() {
        let tx = SpanBatchEip8130TransactionData {
            sender: Some(address!("0x00000000000000000000000000000000000000aa")),
            nonce_key: U256::from(0x1234u64),
            expiry: 99,
            max_priority_fee_per_gas: 1_000_000_000,
            max_fee_per_gas: 5_000_000_000,
            payer: Some(address!("0x00000000000000000000000000000000000000bb")),
            account_changes: vec![AccountChange::Delegation(Delegation {
                target: address!("0x00000000000000000000000000000000000000cc"),
            })],
            calls: vec![
                vec![Call {
                    to: address!("0x00000000000000000000000000000000000000dd"),
                    data: bytes!("deadbeef"),
                }],
                vec![
                    Call { to: Address::ZERO, data: bytes!("01") },
                    Call { to: Address::ZERO, data: bytes!("02") },
                ],
            ],
            metadata: bytes!("c0ffee"),
            sender_authenticator: Bytes::from_static(&[0x11; 20]),
            payer_authenticator: Bytes::from_static(&[0x22; 20]),
        };

        let mut encoded_buf = Vec::new();
        SpanBatchTransactionData::Eip8130(tx.clone()).encode(&mut encoded_buf);

        let decoded = SpanBatchTransactionData::decode(&mut encoded_buf.as_slice()).unwrap();
        let SpanBatchTransactionData::Eip8130(decoded) = decoded else {
            panic!("Expected SpanBatchEip8130TransactionData, got {decoded:?}");
        };

        assert_eq!(tx, decoded);
    }

    #[test]
    fn split_auth_rejects_short_configured_blob() {
        // A configured-path auth blob shorter than the 20-byte account address
        // must error instead of panicking on the slice.
        let blob = Bytes::from(vec![0u8; 19]);
        assert_eq!(
            SpanBatchEip8130TransactionData::split_auth(&blob, true),
            Err(SpanBatchError::Decoding(SpanDecodingError::InvalidAuthData))
        );
    }

    #[test]
    fn split_auth_is_inverse_of_join_auth() {
        // EOA path: authenticator empty, proof is the whole blob.
        let eoa = Bytes::from(vec![7u8; 65]);
        let (auth, proof) = SpanBatchEip8130TransactionData::split_auth(&eoa, false).unwrap();
        assert!(auth.is_empty());
        assert_eq!(proof, eoa);
        assert_eq!(SpanBatchEip8130TransactionData::join_auth(&auth, &proof, false), eoa);

        // Configured path, exactly 20 bytes: proof empty, join rebuilds the blob.
        let exact = Bytes::from(vec![3u8; 20]);
        let (auth, proof) = SpanBatchEip8130TransactionData::split_auth(&exact, true).unwrap();
        assert_eq!(auth, exact);
        assert!(proof.is_empty());
        assert_eq!(SpanBatchEip8130TransactionData::join_auth(&auth, &proof, true), exact);

        // Configured path, 20-byte authenticator + proof.
        let mut full = vec![1u8; 20];
        full.extend_from_slice(&[9u8; 32]);
        let full = Bytes::from(full);
        let (auth, proof) = SpanBatchEip8130TransactionData::split_auth(&full, true).unwrap();
        assert_eq!(&auth[..], &[1u8; 20]);
        assert_eq!(&proof[..], &[9u8; 32]);
        assert_eq!(SpanBatchEip8130TransactionData::join_auth(&auth, &proof, true), full);
    }
}
