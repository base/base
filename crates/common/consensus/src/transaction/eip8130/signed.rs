//! Signed [EIP-8130] Account Abstraction transaction envelope ([`Eip8130Signed`]).
//!
//! [`Eip8130Signed`] wraps a [`TxEip8130`] together with the two opaque byte strings
//! `sender_auth` and `payer_auth` that authenticate the sender and (optional)
//! payer respectively. The wire format is:
//!
//! ```text
//! EIP8130_TX_TYPE || rlp([...TxEip8130 fields..., sender_auth, payer_auth])
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

use crate::transaction::eip8130::{constants::Eip8130Constants, tx::TxEip8130};

/// Signed [EIP-8130] Account Abstraction transaction envelope.
///
/// Holds the unsigned [`TxEip8130`] body plus the two authentication byte
/// strings. The transaction hash is computed at construction and cached.
///
/// [EIP-8130]: https://eips.ethereum.org/EIPS/eip-8130
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Eip8130Signed {
    /// Unsigned transaction body.
    tx: TxEip8130,
    /// Sender authentication payload.
    ///
    /// On the EOA path (`tx.sender == None`) this is a 65-byte ECDSA signature
    /// (`r || s || v`) over [`TxEip8130::sender_signature_hash`].
    /// On the configured-owner path (`tx.sender == Some(_)`) this is
    /// `verifier(20) || verifier_data`.
    sender_auth: Bytes,
    /// Payer authentication payload, or empty for self-pay.
    ///
    /// When `tx.payer.is_some()` this carries the payer's authorization,
    /// formatted as `verifier(20) || verifier_data` and validated against
    /// [`TxEip8130::payer_signature_hash`] (with the resolved sender substituted).
    /// When `tx.payer.is_none()` this is empty.
    payer_auth: Bytes,
    /// Cached EIP-2718 transaction hash (`keccak256(encode_2718(self))`).
    hash: B256,
}

#[cfg(feature = "serde")]
mod serde_impl {
    use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

    use super::{Bytes, Eip8130Signed, TxEip8130};

    #[derive(Serialize, Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Eip8130SignedRepr {
        tx: TxEip8130,
        sender_auth: Bytes,
        payer_auth: Bytes,
    }

    impl Serialize for Eip8130Signed {
        fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
        {
            Eip8130SignedRepr {
                tx: self.tx.clone(),
                sender_auth: self.sender_auth.clone(),
                payer_auth: self.payer_auth.clone(),
            }
            .serialize(serializer)
        }
    }

    impl<'de> Deserialize<'de> for Eip8130Signed {
        fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
        where
            D: Deserializer<'de>,
        {
            let repr = Eip8130SignedRepr::deserialize(deserializer).map_err(de::Error::custom)?;
            Ok(Self::new(repr.tx, repr.sender_auth, repr.payer_auth))
        }
    }
}

#[cfg(feature = "arbitrary")]
impl<'a> arbitrary::Arbitrary<'a> for Eip8130Signed {
    fn arbitrary(u: &mut arbitrary::Unstructured<'a>) -> arbitrary::Result<Self> {
        Ok(Self::new(TxEip8130::arbitrary(u)?, Bytes::arbitrary(u)?, Bytes::arbitrary(u)?))
    }
}

impl Eip8130Signed {
    /// Constructs a new [`Eip8130Signed`] from its parts, computing and caching
    /// the EIP-2718 transaction hash.
    pub fn new(tx: TxEip8130, sender_auth: Bytes, payer_auth: Bytes) -> Self {
        let mut this = Self { tx, sender_auth, payer_auth, hash: B256::ZERO };
        this.hash = this.recompute_hash();
        this
    }

    /// Returns the unsigned transaction body.
    pub const fn tx(&self) -> &TxEip8130 {
        &self.tx
    }

    /// Consumes the envelope and returns the unsigned transaction body.
    pub fn into_tx(self) -> TxEip8130 {
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

    /// Recovers the sender for the EOA-path EIP-8130 transaction using the
    /// **checked** secp256k1 recovery (rejects upper-half `s` values per
    /// EIP-2).
    ///
    /// Returns `Ok(None)` when [`Self::explicit_sender`] is `Some(_)` — the
    /// configured-owner path does not require ecrecover because the sender
    /// address is already in the transaction body.
    ///
    /// Returns `Ok(Some(addr))` when [`TxEip8130::sender`] is `None`: parses
    /// the 65-byte `r || s || v` ECDSA payload in [`Self::sender_auth`] and
    /// recovers the signer against [`TxEip8130::sender_signature_hash`].
    ///
    /// Returns `Err(_)` when the EOA payload is malformed (wrong length or
    /// invalid signature). Callers should treat a missing sender + malformed
    /// `sender_auth` as a hard rejection.
    #[cfg(feature = "k256")]
    pub fn recover_eoa_sender(
        &self,
    ) -> Result<Option<Address>, alloy_consensus::crypto::RecoveryError> {
        self.recover_eoa_sender_with(alloy_consensus::crypto::secp256k1::recover_signer)
    }

    /// Same as [`Self::recover_eoa_sender`] but uses the **unchecked** recovery
    /// path, accepting signatures with non-canonical (upper-half) `s` values.
    ///
    /// Intended for use from `SignerRecoverable::recover_signer_unchecked`
    /// dispatchers where the contract guarantees no upper-half-`s` filtering;
    /// using the checked variant from those paths would silently tighten the
    /// validation contract.
    #[cfg(feature = "k256")]
    pub fn recover_eoa_sender_unchecked(
        &self,
    ) -> Result<Option<Address>, alloy_consensus::crypto::RecoveryError> {
        self.recover_eoa_sender_with(alloy_consensus::crypto::secp256k1::recover_signer_unchecked)
    }

    /// Recovers the sender of this signed transaction by short-circuiting to
    /// [`Self::explicit_sender`] for the configured-owner path and otherwise
    /// running checked EOA ecrecover. Flattens the [`Self::recover_eoa_sender`]
    /// `Option` so call sites in the pooled and envelope `SignerRecoverable`
    /// implementations stay one-liners and cannot drift.
    #[cfg(feature = "k256")]
    pub fn recover_sender(&self) -> Result<Address, alloy_consensus::crypto::RecoveryError> {
        if let Some(addr) = self.explicit_sender() {
            return Ok(addr);
        }
        self.recover_eoa_sender()?.ok_or_else(alloy_consensus::crypto::RecoveryError::new)
    }

    /// Same as [`Self::recover_sender`] but uses the unchecked recovery path,
    /// preserving the upper-half-`s`-accepting contract required by the
    /// `recover_signer_unchecked` and `recover_unchecked_with_buf` dispatchers.
    #[cfg(feature = "k256")]
    pub fn recover_sender_unchecked(
        &self,
    ) -> Result<Address, alloy_consensus::crypto::RecoveryError> {
        if let Some(addr) = self.explicit_sender() {
            return Ok(addr);
        }
        self.recover_eoa_sender_unchecked()?.ok_or_else(alloy_consensus::crypto::RecoveryError::new)
    }

    #[cfg(feature = "k256")]
    fn recover_eoa_sender_with(
        &self,
        recover: impl FnOnce(
            &alloy_primitives::Signature,
            B256,
        ) -> Result<Address, alloy_consensus::crypto::RecoveryError>,
    ) -> Result<Option<Address>, alloy_consensus::crypto::RecoveryError> {
        if self.tx.sender.is_some() {
            return Ok(None);
        }
        let signature = alloy_primitives::Signature::try_from(self.sender_auth.as_ref())
            .map_err(|_| alloy_consensus::crypto::RecoveryError::new())?;
        let hash = self.tx.sender_signature_hash();
        recover(&signature, hash).map(Some)
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
        let tx = TxEip8130::rlp_decode_fields(buf)?;
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

impl Encodable for Eip8130Signed {
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

impl Decodable for Eip8130Signed {
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

impl Typed2718 for Eip8130Signed {
    fn ty(&self) -> u8 {
        Eip8130Constants::EIP8130_TX_TYPE
    }
}

impl IsTyped2718 for Eip8130Signed {
    fn is_type(ty: u8) -> bool {
        ty == Eip8130Constants::EIP8130_TX_TYPE
    }
}

impl Encodable2718 for Eip8130Signed {
    fn type_flag(&self) -> Option<u8> {
        Some(Eip8130Constants::EIP8130_TX_TYPE)
    }

    fn encode_2718_len(&self) -> usize {
        1 + self.rlp_encoded_signed_length()
    }

    fn encode_2718(&self, out: &mut dyn BufMut) {
        out.put_u8(Eip8130Constants::EIP8130_TX_TYPE);
        self.rlp_encode_signed(out);
    }

    fn trie_hash(&self) -> B256 {
        self.hash
    }
}

impl Decodable2718 for Eip8130Signed {
    fn typed_decode(ty: u8, buf: &mut &[u8]) -> Eip2718Result<Self> {
        if ty != Eip8130Constants::EIP8130_TX_TYPE {
            return Err(Eip2718Error::UnexpectedType(ty));
        }
        Self::rlp_decode_signed(buf).map_err(Into::into)
    }

    fn fallback_decode(_buf: &mut &[u8]) -> Eip2718Result<Self> {
        Err(Eip2718Error::UnexpectedType(0))
    }
}

impl InMemorySize for Eip8130Signed {
    fn size(&self) -> usize {
        InMemorySize::size(&self.tx) + self.sender_auth.len() + self.payer_auth.len()
    }
}

impl Transaction for Eip8130Signed {
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
    use crate::transaction::eip8130::{
        account_changes::{AccountChange, Delegation},
        call::Call,
    };

    fn sample_signed(payer_present: bool) -> Eip8130Signed {
        let tx = TxEip8130 {
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
        Eip8130Signed::new(
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
        assert_eq!(buf[0], Eip8130Constants::EIP8130_TX_TYPE);
        assert_eq!(buf.len(), signed.encode_2718_len());

        let decoded = Eip8130Signed::decode_2718(&mut buf.as_slice()).unwrap();
        assert_eq!(signed, decoded);
    }

    #[test]
    fn eip2718_roundtrip_sponsored() {
        let signed = sample_signed(true);
        let mut buf = Vec::new();
        signed.encode_2718(&mut buf);
        let decoded = Eip8130Signed::decode_2718(&mut buf.as_slice()).unwrap();
        assert_eq!(signed, decoded);
    }

    #[test]
    fn rlp_envelope_roundtrip() {
        let signed = sample_signed(true);
        let mut buf = Vec::new();
        signed.encode(&mut buf);
        let decoded = Eip8130Signed::decode(&mut buf.as_slice()).unwrap();
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
        assert_eq!(signed.ty(), Eip8130Constants::EIP8130_TX_TYPE);
        assert_eq!(signed.type_flag(), Some(Eip8130Constants::EIP8130_TX_TYPE));
    }

    #[test]
    fn typed_decode_rejects_wrong_type() {
        let signed = sample_signed(false);
        let mut buf = Vec::new();
        signed.rlp_encode_signed(&mut buf);
        let res = Eip8130Signed::typed_decode(0x00, &mut buf.as_slice());
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

    #[cfg(feature = "k256")]
    #[test]
    fn recover_eoa_sender_returns_none_for_configured_owner() {
        let signed = sample_signed(false);
        assert!(signed.explicit_sender().is_some());
        assert_eq!(signed.recover_eoa_sender().unwrap(), None);
    }

    #[cfg(feature = "k256")]
    #[test]
    fn recover_eoa_sender_recovers_eoa_signer() {
        use alloy_signer::SignerSync;
        use alloy_signer_local::PrivateKeySigner;

        let signer = PrivateKeySigner::random();
        let expected = signer.address();

        let mut tx = sample_signed(false).into_tx();
        tx.sender = None;
        let signature = signer.sign_hash_sync(&tx.sender_signature_hash()).unwrap();
        let sender_auth = Bytes::from(signature.as_bytes().to_vec());
        let signed = Eip8130Signed::new(tx, sender_auth, Bytes::new());

        assert_eq!(signed.recover_eoa_sender().unwrap(), Some(expected));
    }

    #[cfg(feature = "k256")]
    #[test]
    fn recover_eoa_sender_rejects_malformed_payload() {
        let mut tx = sample_signed(false).into_tx();
        tx.sender = None;
        // 64 bytes is one short of a valid ECDSA r||s||v payload.
        let signed = Eip8130Signed::new(tx, Bytes::from(vec![0u8; 64]), Bytes::new());
        assert!(signed.recover_eoa_sender().is_err());
    }

    #[cfg(feature = "k256")]
    #[test]
    fn recover_eoa_sender_unchecked_accepts_high_s_signature() {
        use alloy_primitives::U256;
        use alloy_signer::SignerSync;
        use alloy_signer_local::PrivateKeySigner;

        // secp256k1 curve order N.
        const SECP256K1_N: U256 = U256::from_be_slice(&[
            0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xfe, 0xba, 0xae, 0xdc, 0xe6, 0xaf, 0x48, 0xa0, 0x3b, 0xbf, 0xd2, 0x5e, 0x8c,
            0xd0, 0x36, 0x41, 0x41,
        ]);

        let signer = PrivateKeySigner::random();
        let expected = signer.address();

        let mut tx = sample_signed(false).into_tx();
        tx.sender = None;
        let hash = tx.sender_signature_hash();

        // Sign normally (low-s, EIP-2 canonical), then flip s into the upper half
        // by replacing it with N - s and inverting parity.
        let canonical = signer.sign_hash_sync(&hash).unwrap();
        let high_s_sig = alloy_primitives::Signature::new(
            canonical.r(),
            SECP256K1_N - canonical.s(),
            !canonical.v(),
        );
        let signed =
            Eip8130Signed::new(tx, Bytes::from(high_s_sig.as_bytes().to_vec()), Bytes::new());

        // The checked recovery rejects the high-s form (EIP-2);
        // the unchecked recovery accepts it and recovers the same address.
        assert!(signed.recover_eoa_sender().is_err());
        assert_eq!(signed.recover_eoa_sender_unchecked().unwrap(), Some(expected));
    }

    #[cfg(feature = "k256")]
    #[test]
    fn recover_eoa_sender_unchecked_returns_none_for_configured_owner() {
        let signed = sample_signed(false);
        assert!(signed.explicit_sender().is_some());
        assert_eq!(signed.recover_eoa_sender_unchecked().unwrap(), None);
    }

    #[cfg(feature = "serde")]
    #[test]
    fn serde_roundtrip_recomputes_hash() {
        let signed = sample_signed(true);
        let json = serde_json::to_string(&signed).unwrap();

        assert!(!json.contains("\"hash\""));

        let decoded: Eip8130Signed = serde_json::from_str(&json).unwrap();
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

        let decoded: Eip8130Signed = serde_json::from_value(value).unwrap();
        assert_eq!(*decoded.hash(), *signed.hash());
        assert_ne!(*decoded.hash(), B256::ZERO);
    }
}
