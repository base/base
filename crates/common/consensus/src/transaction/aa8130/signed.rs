//! Signed [EIP-8130] Account Abstraction transaction envelope ([`AaSigned`]).
//!
//! [`AaSigned`] wraps a [`TxAa8130`] together with the two opaque byte strings
//! `sender_auth` and `payer_auth` that authenticate the sender and (optional)
//! payer respectively. The wire format is:
//!
//! ```text
//! AA_TX_TYPE || rlp([...TxAa8130 fields..., sender_auth, payer_auth])
//! ```
//!
//! [EIP-8130]: https://eips.ethereum.org/EIPS/eip-8130

use alloc::vec::Vec;

use alloy_consensus::{InMemorySize, Transaction, Typed2718};
use alloy_eips::{
    eip2718::{Decodable2718, Eip2718Error, Eip2718Result, Encodable2718, IsTyped2718},
    eip2930::AccessList,
    eip7702::SignedAuthorization,
};
use alloy_primitives::{Address, B256, Bytes, ChainId, TxKind, U256, bytes::BufMut, keccak256};
use alloy_rlp::{Decodable, Encodable, Header, length_of_length};

use crate::transaction::aa8130::{constants::Aa8130Constants, tx::TxAa8130};

/// Signed [EIP-8130] Account Abstraction transaction envelope.
///
/// Holds the unsigned [`TxAa8130`] body plus the two authentication byte
/// strings. The transaction hash is computed at construction and cached.
///
/// [EIP-8130]: https://eips.ethereum.org/EIPS/eip-8130
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AaSigned {
    /// Unsigned transaction body.
    tx: TxAa8130,
    /// Sender authentication payload.
    ///
    /// On the EOA path (`tx.sender == None`) this is a 65-byte ECDSA signature
    /// (`r || s || v`) over [`TxAa8130::sender_signature_hash`].
    /// On the configured-owner path (`tx.sender == Some(_)`) this is
    /// `verifier(20) || verifier_data`.
    sender_auth: Bytes,
    /// Payer authentication payload, or empty for self-pay.
    ///
    /// When `tx.payer.is_some()` this carries the payer's authorization,
    /// formatted as `verifier(20) || verifier_data` and validated against
    /// [`TxAa8130::payer_signature_hash`] (with the resolved sender substituted).
    /// When `tx.payer.is_none()` this is empty.
    payer_auth: Bytes,
    /// Cached EIP-2718 transaction hash (`keccak256(encode_2718(self))`).
    hash: B256,
}

#[cfg(feature = "serde")]
mod serde_impl {
    use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

    use super::{AaSigned, Bytes, TxAa8130};

    #[derive(Serialize, Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct AaSignedRepr {
        tx: TxAa8130,
        sender_auth: Bytes,
        payer_auth: Bytes,
    }

    impl Serialize for AaSigned {
        fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
        {
            AaSignedRepr {
                tx: self.tx.clone(),
                sender_auth: self.sender_auth.clone(),
                payer_auth: self.payer_auth.clone(),
            }
            .serialize(serializer)
        }
    }

    impl<'de> Deserialize<'de> for AaSigned {
        fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
        where
            D: Deserializer<'de>,
        {
            let repr = AaSignedRepr::deserialize(deserializer).map_err(de::Error::custom)?;
            Ok(Self::new(repr.tx, repr.sender_auth, repr.payer_auth))
        }
    }
}

#[cfg(feature = "arbitrary")]
impl<'a> arbitrary::Arbitrary<'a> for AaSigned {
    fn arbitrary(u: &mut arbitrary::Unstructured<'a>) -> arbitrary::Result<Self> {
        Ok(Self::new(TxAa8130::arbitrary(u)?, Bytes::arbitrary(u)?, Bytes::arbitrary(u)?))
    }
}

impl AaSigned {
    /// Constructs a new [`AaSigned`] from its parts, computing and caching
    /// the EIP-2718 transaction hash.
    pub fn new(tx: TxAa8130, sender_auth: Bytes, payer_auth: Bytes) -> Self {
        let mut this = Self { tx, sender_auth, payer_auth, hash: B256::ZERO };
        this.hash = this.recompute_hash();
        this
    }

    /// Returns the unsigned transaction body.
    pub const fn tx(&self) -> &TxAa8130 {
        &self.tx
    }

    /// Consumes the envelope and returns the unsigned transaction body.
    pub fn into_tx(self) -> TxAa8130 {
        self.tx
    }

    /// Returns the sender authentication payload.
    pub const fn sender_auth(&self) -> &Bytes {
        &self.sender_auth
    }

    /// Returns the payer authentication payload.
    pub const fn payer_auth(&self) -> &Bytes {
        &self.payer_auth
    }

    /// Returns the cached EIP-2718 transaction hash.
    pub const fn hash(&self) -> &B256 {
        &self.hash
    }

    fn recompute_hash(&self) -> B256 {
        let mut buf = Vec::with_capacity(self.encode_2718_len());
        self.encode_2718(&mut buf);
        keccak256(&buf)
    }

    /// Returns the sender address if it is explicitly provided by the
    /// transaction body (configured-owner path).
    pub const fn explicit_sender(&self) -> Option<Address> {
        self.tx.sender
    }

    fn rlp_payload_length(&self) -> usize {
        self.tx.rlp_encoded_fields_length() + self.sender_auth.length() + self.payer_auth.length()
    }

    fn rlp_header(&self) -> Header {
        Header { list: true, payload_length: self.rlp_payload_length() }
    }

    /// RLP-encodes the signed body (with list header) as
    /// `rlp([...tx fields..., sender_auth, payer_auth])`.
    pub fn rlp_encode_signed(&self, out: &mut dyn BufMut) {
        self.rlp_header().encode(out);
        self.tx.rlp_encode_fields(out);
        self.sender_auth.encode(out);
        self.payer_auth.encode(out);
    }

    fn rlp_encoded_signed_length(&self) -> usize {
        let payload = self.rlp_payload_length();
        length_of_length(payload) + payload
    }

    /// RLP-decodes the signed body produced by [`Self::rlp_encode_signed`].
    pub fn rlp_decode_signed(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        let header = Header::decode(buf)?;
        if !header.list {
            return Err(alloy_rlp::Error::UnexpectedString);
        }
        let started = buf.len();
        let tx = TxAa8130::rlp_decode_fields(buf)?;
        let sender_auth = Bytes::decode(buf)?;
        let payer_auth = Bytes::decode(buf)?;
        let consumed = started - buf.len();
        if consumed != header.payload_length {
            return Err(alloy_rlp::Error::ListLengthMismatch {
                expected: header.payload_length,
                got: consumed,
            });
        }
        Ok(Self::new(tx, sender_auth, payer_auth))
    }
}

impl Encodable for AaSigned {
    fn encode(&self, out: &mut dyn BufMut) {
        let len = self.encode_2718_len();
        Header { list: false, payload_length: len }.encode(out);
        self.encode_2718(out);
    }

    fn length(&self) -> usize {
        let inner = self.encode_2718_len();
        length_of_length(inner) + inner
    }
}

impl Decodable for AaSigned {
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        let header = Header::decode(buf)?;
        if header.list {
            return Err(alloy_rlp::Error::Custom("expected EIP-2718 envelope, got list"));
        }
        if buf.len() < header.payload_length {
            return Err(alloy_rlp::Error::InputTooShort);
        }
        let (mut payload, rest) = buf.split_at(header.payload_length);
        *buf = rest;
        let decoded = Self::decode_2718(&mut payload)
            .map_err(|_| alloy_rlp::Error::Custom("invalid EIP-8130 envelope"))?;
        if !payload.is_empty() {
            return Err(alloy_rlp::Error::Custom("trailing bytes in EIP-8130 envelope"));
        }
        Ok(decoded)
    }
}

impl Typed2718 for AaSigned {
    fn ty(&self) -> u8 {
        Aa8130Constants::AA_TX_TYPE
    }
}

impl IsTyped2718 for AaSigned {
    fn is_type(ty: u8) -> bool {
        ty == Aa8130Constants::AA_TX_TYPE
    }
}

impl Encodable2718 for AaSigned {
    fn type_flag(&self) -> Option<u8> {
        Some(Aa8130Constants::AA_TX_TYPE)
    }

    fn encode_2718_len(&self) -> usize {
        1 + self.rlp_encoded_signed_length()
    }

    fn encode_2718(&self, out: &mut dyn BufMut) {
        out.put_u8(Aa8130Constants::AA_TX_TYPE);
        self.rlp_encode_signed(out);
    }

    fn trie_hash(&self) -> B256 {
        self.hash
    }
}

impl Decodable2718 for AaSigned {
    fn typed_decode(ty: u8, buf: &mut &[u8]) -> Eip2718Result<Self> {
        if ty != Aa8130Constants::AA_TX_TYPE {
            return Err(Eip2718Error::UnexpectedType(ty));
        }
        Self::rlp_decode_signed(buf).map_err(Into::into)
    }

    fn fallback_decode(_buf: &mut &[u8]) -> Eip2718Result<Self> {
        Err(Eip2718Error::UnexpectedType(0))
    }
}

impl InMemorySize for AaSigned {
    fn size(&self) -> usize {
        InMemorySize::size(&self.tx) + self.sender_auth.len() + self.payer_auth.len()
    }
}

impl Transaction for AaSigned {
    fn chain_id(&self) -> Option<ChainId> {
        self.tx.chain_id()
    }

    fn nonce(&self) -> u64 {
        self.tx.nonce()
    }

    fn gas_limit(&self) -> u64 {
        self.tx.gas_limit()
    }

    fn gas_price(&self) -> Option<u128> {
        self.tx.gas_price()
    }

    fn max_fee_per_gas(&self) -> u128 {
        self.tx.max_fee_per_gas()
    }

    fn max_priority_fee_per_gas(&self) -> Option<u128> {
        self.tx.max_priority_fee_per_gas()
    }

    fn max_fee_per_blob_gas(&self) -> Option<u128> {
        self.tx.max_fee_per_blob_gas()
    }

    fn priority_fee_or_price(&self) -> u128 {
        self.tx.priority_fee_or_price()
    }

    fn effective_gas_price(&self, base_fee: Option<u64>) -> u128 {
        self.tx.effective_gas_price(base_fee)
    }

    fn is_dynamic_fee(&self) -> bool {
        self.tx.is_dynamic_fee()
    }

    fn kind(&self) -> TxKind {
        self.tx.kind()
    }

    fn is_create(&self) -> bool {
        self.tx.is_create()
    }

    fn value(&self) -> U256 {
        self.tx.value()
    }

    fn input(&self) -> &Bytes {
        self.tx.input()
    }

    fn access_list(&self) -> Option<&AccessList> {
        self.tx.access_list()
    }

    fn blob_versioned_hashes(&self) -> Option<&[B256]> {
        self.tx.blob_versioned_hashes()
    }

    fn authorization_list(&self) -> Option<&[SignedAuthorization]> {
        self.tx.authorization_list()
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{address, bytes};

    use super::*;
    use crate::transaction::aa8130::{
        account_changes::{AccountChange, Delegation},
        call::Call,
    };

    fn sample_signed(payer_present: bool) -> AaSigned {
        let tx = TxAa8130 {
            chain_id: 8453,
            sender: Some(address!("0x00000000000000000000000000000000000000aa")),
            nonce_key: U256::from(7u64),
            nonce_sequence: 3,
            expiry: 0,
            max_priority_fee_per_gas: 1_000_000_000,
            max_fee_per_gas: 5_000_000_000,
            gas_limit: 250_000,
            account_changes: vec![AccountChange::Delegation(Delegation { target: Address::ZERO })],
            calls: vec![vec![Call {
                to: address!("0x00000000000000000000000000000000000000bb"),
                data: bytes!("01020304"),
            }]],
            payer: if payer_present {
                Some(address!("0x00000000000000000000000000000000000000cc"))
            } else {
                None
            },
        };
        AaSigned::new(
            tx,
            bytes!("deadbeef"),
            if payer_present { bytes!("cafebabe") } else { Bytes::new() },
        )
    }

    #[test]
    fn eip2718_roundtrip_self_pay() {
        let signed = sample_signed(false);
        let mut buf = Vec::new();
        signed.encode_2718(&mut buf);
        assert_eq!(buf[0], Aa8130Constants::AA_TX_TYPE);
        assert_eq!(buf.len(), signed.encode_2718_len());

        let decoded = AaSigned::decode_2718(&mut buf.as_slice()).unwrap();
        assert_eq!(signed, decoded);
    }

    #[test]
    fn eip2718_roundtrip_sponsored() {
        let signed = sample_signed(true);
        let mut buf = Vec::new();
        signed.encode_2718(&mut buf);
        let decoded = AaSigned::decode_2718(&mut buf.as_slice()).unwrap();
        assert_eq!(signed, decoded);
    }

    #[test]
    fn rlp_envelope_roundtrip() {
        let signed = sample_signed(true);
        let mut buf = Vec::new();
        signed.encode(&mut buf);
        let decoded = AaSigned::decode(&mut buf.as_slice()).unwrap();
        assert_eq!(signed, decoded);
    }

    #[test]
    fn hash_is_keccak_of_eip2718_payload() {
        let signed = sample_signed(false);
        let mut buf = Vec::new();
        signed.encode_2718(&mut buf);
        assert_eq!(*signed.hash(), keccak256(&buf));
    }

    #[test]
    fn hash_is_deterministic() {
        let signed = sample_signed(false);
        assert_eq!(signed.hash(), signed.hash());
    }

    #[test]
    fn ty_byte() {
        let signed = sample_signed(false);
        assert_eq!(signed.ty(), Aa8130Constants::AA_TX_TYPE);
        assert_eq!(signed.type_flag(), Some(Aa8130Constants::AA_TX_TYPE));
    }

    #[test]
    fn typed_decode_rejects_wrong_type() {
        let signed = sample_signed(false);
        let mut buf = Vec::new();
        signed.rlp_encode_signed(&mut buf);
        let res = AaSigned::typed_decode(0x00, &mut buf.as_slice());
        assert!(res.is_err());
    }

    #[test]
    fn explicit_sender_returns_field() {
        let signed = sample_signed(false);
        assert_eq!(
            signed.explicit_sender(),
            Some(address!("0x00000000000000000000000000000000000000aa"))
        );
    }

    #[cfg(feature = "serde")]
    #[test]
    fn serde_roundtrip_recomputes_hash() {
        let signed = sample_signed(true);
        let json = serde_json::to_string(&signed).unwrap();

        assert!(!json.contains("\"hash\""));

        let decoded: AaSigned = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, signed);
        assert_eq!(decoded.hash(), signed.hash());
    }

    #[cfg(feature = "serde")]
    #[test]
    fn serde_deserialize_computes_hash_from_payload() {
        let signed = sample_signed(false);
        let mut value = serde_json::to_value(&signed).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .insert("hash".to_string(), serde_json::Value::String(format!("{:?}", B256::ZERO)));

        let decoded: AaSigned = serde_json::from_value(value).unwrap();
        assert_eq!(*decoded.hash(), *signed.hash());
        assert_ne!(*decoded.hash(), B256::ZERO);
    }
}
